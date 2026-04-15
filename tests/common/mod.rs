// Shared test utilities -- not all functions are used by every test file.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;

/// Set when any test detects an Apple 503 rate-limit response.
static RATE_LIMITED: AtomicBool = AtomicBool::new(false);

const RATE_LIMIT_MARKER: &str = "503 Service Temporarily Unavailable";
const AUTH_FAILURE_MARKER: &str = "Invalid email/password combination";

/// Cached auth credentials for reactive session refresh mid-run.
static AUTH_CREDS: OnceLock<(String, String, PathBuf)> = OnceLock::new();

/// Load `.env` exactly once across all test functions.
fn init_env() {
    static INIT: OnceLock<()> = OnceLock::new();
    INIT.get_or_init(|| {
        let _ = dotenvy::from_filename(".env");
        install_rate_limit_hook();
    });
}

/// Install a panic hook that aborts the test suite on Apple 503 responses.
///
/// When `assert_cmd` assertions fail, the panic message includes the full
/// stderr output. If that output contains a 503 response, continuing is
/// pointless — every subsequent test will also 503 due to session
/// invalidation. We abort immediately to save time and rate-limit budget.
fn install_rate_limit_hook() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let msg = info
            .payload()
            .downcast_ref::<String>()
            .map(|s| s.as_str())
            .or_else(|| info.payload().downcast_ref::<&str>().copied())
            .unwrap_or("");

        if msg.contains(RATE_LIMIT_MARKER) {
            RATE_LIMITED.store(true, Ordering::SeqCst);
            eprintln!("\n*** ABORTING: Apple 503 rate limit detected ***");
            eprintln!("*** Wait 10-15 minutes before retrying.      ***\n");
            std::process::exit(1);
        }

        default(info);
    }));
}

/// Sleep between auth tests to reduce Apple API rate-limit risk.
///
/// With session reuse (accountLogin fallback), most invocations avoid SRP,
/// but spacing API calls is still polite. Default: 2 seconds. Override with
/// `TEST_THROTTLE_SECS` env var (0 to disable).
fn throttle() {
    static FIRST: AtomicBool = AtomicBool::new(true);
    if FIRST.swap(false, Ordering::SeqCst) {
        return; // no delay before the very first test
    }
    let secs: u64 = std::env::var("TEST_THROTTLE_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(2);
    if secs > 0 {
        std::thread::sleep(std::time::Duration::from_secs(secs));
    }
}

/// Build an `assert_cmd::Command` for the kei binary.
///
/// Loads `.env` from the repo root (if present) so that `ICLOUD_USERNAME`
/// and `ICLOUD_PASSWORD` are available to the child process.
pub fn cmd() -> assert_cmd::Command {
    init_env();
    assert_cmd::cargo_bin_cmd!("kei")
}

/// Return credentials from the environment, panicking if not set.
///
/// All callers are `#[ignore]` tests — if someone explicitly opts in via
/// `--ignored` without configuring credentials, a loud failure is correct.
fn require_creds() -> (String, String) {
    init_env();
    let username = std::env::var("ICLOUD_USERNAME")
        .expect("ICLOUD_USERNAME must be set (see tests/README.md)");
    let password = std::env::var("ICLOUD_PASSWORD")
        .expect("ICLOUD_PASSWORD must be set (see tests/README.md)");
    assert!(!username.is_empty(), "ICLOUD_USERNAME must not be empty");
    assert!(!password.is_empty(), "ICLOUD_PASSWORD must not be empty");
    (username, password)
}

/// Path to the shared pre-authenticated cookie directory.
///
/// Reads `ICLOUD_TEST_COOKIE_DIR` from the environment, falling back to
/// `{repo_root}/.test-cookies/`.
#[allow(dead_code)]
pub fn cookie_dir() -> PathBuf {
    init_env();
    if let Ok(dir) = std::env::var("ICLOUD_TEST_COOKIE_DIR") {
        return PathBuf::from(dir);
    }
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest.join(".test-cookies")
}

/// Run `--auth-only` once to ensure the session cookies are fresh.
///
/// If the pre-existing session has expired, this re-authenticates and
/// refreshes the cookies. If authentication genuinely fails (wrong
/// password, rate-limited), aborts the suite early rather than failing
/// tests one by one.
fn ensure_session(username: &str, password: &str, cookie_dir: &Path) {
    static ENSURED: OnceLock<()> = OnceLock::new();
    ENSURED.get_or_init(|| {
        // Skip SRP if session file is fresh (< 1 hour old). This avoids
        // burning rate-limit budget on every test run when cookies are valid.
        let sanitized: String = username
            .chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .collect();
        let session_file = cookie_dir.join(format!("{sanitized}.session"));
        if let Ok(meta) = std::fs::metadata(&session_file) {
            let is_fresh = meta
                .modified()
                .ok()
                .and_then(|m| m.elapsed().ok())
                .is_some_and(|age| age < std::time::Duration::from_secs(48 * 3600));
            if is_fresh {
                eprintln!("Session file is fresh, skipping SRP validation.");
                return;
            }
        }

        eprintln!("Validating authentication session (login)...");
        let output = assert_cmd::cargo_bin_cmd!("kei")
            .args([
                "login",
                "--username",
                username,
                "--password",
                password,
                "--data-dir",
                cookie_dir.to_str().unwrap(),
            ])
            .timeout(std::time::Duration::from_secs(90))
            .output()
            .expect("failed to run login session validation");

        if output.status.success() {
            eprintln!("Session OK.");
            return;
        }

        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains(RATE_LIMIT_MARKER) {
            RATE_LIMITED.store(true, Ordering::SeqCst);
            eprintln!("\n*** ABORTING: Apple 503 rate limit during session validation ***");
            std::process::exit(1);
        }

        panic!("Session validation (login) failed — credentials may be invalid.\nstderr: {stderr}");
    });
}

/// Refresh the authentication session by running `--auth-only`.
///
/// Called reactively when a test command fails with an authentication error
/// mid-run (stale session). Panics if the refresh itself fails.
fn refresh_auth() {
    let (username, password, cookie_dir) = AUTH_CREDS
        .get()
        .expect("refresh_auth called before require_preauth");

    eprintln!("Running login to refresh session...");
    let output = assert_cmd::cargo_bin_cmd!("kei")
        .args([
            "login",
            "--username",
            username,
            "--password",
            password,
            "--data-dir",
            cookie_dir.to_str().unwrap(),
        ])
        .timeout(std::time::Duration::from_secs(90))
        .output()
        .expect("failed to run login");

    if output.status.success() {
        eprintln!("Session refreshed OK.");
        return;
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    if stderr.contains(RATE_LIMIT_MARKER) {
        RATE_LIMITED.store(true, Ordering::SeqCst);
        eprintln!("\n*** ABORTING: Apple 503 rate limit during auth refresh ***");
        std::process::exit(1);
    }

    panic!("Auth refresh (login) failed — aborting.\nstderr: {stderr}");
}

/// Run a test body with automatic auth retry on stale-session errors.
///
/// If the test panics with an "Invalid email/password combination" error,
/// refreshes the session via `--auth-only` and retries once. If the retry
/// also hits the same auth error, aborts the entire test suite.
///
/// Does **not** retry on 503 rate limits or other errors.
#[allow(dead_code)]
pub fn with_auth_retry(f: impl Fn()) {
    use std::panic::{catch_unwind, resume_unwind, AssertUnwindSafe};

    match catch_unwind(AssertUnwindSafe(&f)) {
        Ok(()) => {}
        Err(payload) => {
            let msg = payload
                .downcast_ref::<String>()
                .map(|s| s.as_str())
                .or_else(|| payload.downcast_ref::<&str>().copied())
                .unwrap_or("");

            if !msg.contains(AUTH_FAILURE_MARKER) {
                resume_unwind(payload);
            }

            eprintln!("Auth failure detected in test, refreshing session...");
            refresh_auth();
            eprintln!("Retrying test after auth refresh...");

            match catch_unwind(AssertUnwindSafe(&f)) {
                Ok(()) => {}
                Err(retry_payload) => {
                    let retry_msg = retry_payload
                        .downcast_ref::<String>()
                        .map(|s| s.as_str())
                        .or_else(|| retry_payload.downcast_ref::<&str>().copied())
                        .unwrap_or("");

                    if retry_msg.contains(AUTH_FAILURE_MARKER) {
                        eprintln!(
                            "\n*** ABORTING: Auth failure persists after session refresh ***"
                        );
                        std::process::exit(1);
                    }

                    resume_unwind(retry_payload);
                }
            }
        }
    }
}

/// Require a pre-authenticated session. Returns `(username, password, cookie_dir)`.
///
/// All tests share the same cookie directory so only one Apple API session
/// is used per test run. **Auth-requiring tests must run single-threaded:**
///
/// ```sh
/// cargo test --test sync -- --ignored --test-threads=1
/// ```
///
/// On the first call, runs `--auth-only` to validate (and refresh if needed)
/// the session cookies. This prevents stale-session failures mid-run.
///
/// Panics if credentials are not configured or session validation fails.
#[allow(dead_code)]
pub fn require_preauth() -> (String, String, PathBuf) {
    if RATE_LIMITED.load(Ordering::SeqCst) {
        eprintln!("\n*** ABORTING: Apple 503 rate limit detected in earlier test ***");
        std::process::exit(1);
    }
    throttle();
    let (username, password) = require_creds();
    let dir = cookie_dir();
    AUTH_CREDS.get_or_init(|| (username.clone(), password.clone(), dir.clone()));
    ensure_session(&username, &password, &dir);
    (username, password, dir)
}

/// Recursively collect all regular files under `dir`, sorted for deterministic ordering.
#[allow(dead_code)]
pub fn walkdir(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                files.extend(walkdir(&path));
            } else if path.is_file() {
                files.push(path);
            }
        }
    }
    files.sort();
    files
}
