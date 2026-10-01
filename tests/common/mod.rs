// Shared test utilities -- not all functions are used by every test file.
#![allow(dead_code)]

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

pub mod live_selection;

/// Set when any test detects an Apple 503 rate-limit response.
static RATE_LIMITED: AtomicBool = AtomicBool::new(false);

const RATE_LIMIT_MARKER: &str = "503 Service Temporarily Unavailable";
const AUTH_FAILURE_MARKER: &str = "Invalid email/password combination";
const CLOUDKIT_STALE_SESSION_MARKER: &str = "HTTP 401 for https://";

/// Cached auth credentials for reactive session refresh mid-run.
static AUTH_CREDS: OnceLock<(String, String, PathBuf)> = OnceLock::new();

thread_local! {
    /// Keep every live test state directory alive for the lifetime of its test
    /// thread. A test may call `require_preauth` more than once; retaining all
    /// directories prevents a later call from deleting an earlier auth source.
    static ISOLATED_DATA_DIRS: RefCell<Vec<tempfile::TempDir>> = const { RefCell::new(Vec::new()) };
}

fn copy_auth_material(username: &str, source: &Path, destination: &Path) {
    let sanitized: String = username
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect();
    for name in [
        sanitized.clone(),
        format!("{sanitized}.session"),
        format!("{sanitized}.cache"),
    ] {
        let source_path = source.join(&name);
        if source_path.is_file() {
            std::fs::copy(&source_path, destination.join(&name)).unwrap_or_else(|error| {
                panic!(
                    "copy live-test auth material {}: {error}",
                    source_path.display()
                )
            });
        }
    }
}

fn isolated_data_dir(username: &str, shared_cookie_dir: &Path) -> PathBuf {
    ISOLATED_DATA_DIRS.with(|dirs| {
        let dir = tempfile::Builder::new()
            .prefix("kei-live-state-")
            .tempdir()
            .expect("create isolated live-test data dir");
        copy_auth_material(username, shared_cookie_dir, dir.path());
        let path = dir.path().to_path_buf();
        dirs.borrow_mut().push(dir);
        path
    })
}

fn refresh_isolated_auth_material(username: &str, shared_cookie_dir: &Path) {
    ISOLATED_DATA_DIRS.with(|dirs| {
        for dir in dirs.borrow().iter() {
            copy_auth_material(username, shared_cookie_dir, dir.path());
        }
    });
}

#[test]
fn repeated_isolated_data_dirs_remain_available() {
    let source = tempfile::tempdir().expect("create auth source");
    std::fs::write(source.path().join("userexamplecom.session"), b"session")
        .expect("write auth source");

    let first = isolated_data_dir("user@example.com", source.path());
    let second = isolated_data_dir("user@example.com", source.path());

    assert_ne!(first, second);
    assert_eq!(
        std::fs::read(first.join("userexamplecom.session")).expect("read first auth copy"),
        b"session"
    );
    assert_eq!(
        std::fs::read(second.join("userexamplecom.session")).expect("read second auth copy"),
        b"session"
    );
}

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
///
/// NOTE: this is the **one** intentional `thread::sleep` in the suite —
/// the test-review framework's "use `tokio::time::pause/advance` instead
/// of `thread::sleep`" rule applies to in-test synchronization (waiting
/// for a tokio task to make progress). Here the sleep is a wall-clock
/// rate-limit guard against Apple's auth endpoints, which see real
/// elapsed time and wouldn't be fooled by a paused tokio clock. Do not
/// migrate to `tokio::time::sleep` without a concrete plan to keep the
/// inter-test spacing visible to Apple.
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
    let dir = if let Ok(dir) = std::env::var("ICLOUD_TEST_COOKIE_DIR") {
        // Expand ~ since not all shells do it for env vars (e.g. fish)
        if let Some(rest) = dir.strip_prefix("~/") {
            dirs::home_dir()
                .expect("could not determine home directory")
                .join(rest)
        } else {
            PathBuf::from(dir)
        }
    } else {
        let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        manifest.join(".test-cookies")
    };
    std::fs::create_dir_all(&dir).expect("create cookie dir");
    dir
}

/// Run `kei login` once to make sure the session cookies are fresh.
///
/// If the pre-existing session has expired, this re-authenticates and
/// refreshes the cookies. If authentication genuinely fails (wrong
/// password, rate-limited), aborts the suite early rather than failing
/// tests one by one.
fn ensure_session(username: &str, password: &str, cookie_dir: &Path) {
    static ENSURED: OnceLock<()> = OnceLock::new();
    ENSURED.get_or_init(|| {
        // Skip SRP if session file is fresh AND the cookie file contains
        // the WEBAUTH-TOKEN cookie (set only after 2FA trust is established).
        // Without this check, a session that lost trust (e.g., after a 421
        // storm) would be considered "fresh" but fail with 2FA required.
        let sanitized: String = username
            .chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .collect();
        let session_file = cookie_dir.join(format!("{sanitized}.session"));
        let cookie_file = cookie_dir.join(&sanitized);
        let session_fresh = std::fs::metadata(&session_file)
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|m| m.elapsed().ok())
            .is_some_and(|age| age < std::time::Duration::from_secs(48 * 3600));
        let has_trust_cookie = std::fs::read_to_string(&cookie_file)
            .ok()
            .is_some_and(|c| c.contains("X-APPLE-WEBAUTH-TOKEN"));
        if session_fresh && has_trust_cookie {
            eprintln!("Session file is fresh and trusted, skipping SRP validation.");
            return;
        }
        if session_fresh && !has_trust_cookie {
            eprintln!("Session file is fresh but missing trust cookie, re-validating...");
        }

        eprintln!("Validating authentication session (login)...");
        let output = assert_cmd::cargo_bin_cmd!("kei")
            .env("ICLOUD_USERNAME", username)
            .env("KEI_DATA_DIR", cookie_dir)
            .args(["login", "--password", password])
            .timeout(std::time::Duration::from_secs(90))
            .output()
            .expect("failed to run login session validation");

        if output.status.success() {
            // Verify the login actually established trust (not just SRP success
            // without 2FA completion).
            let has_token = std::fs::read_to_string(&cookie_file)
                .ok()
                .is_some_and(|c| c.contains("X-APPLE-WEBAUTH-TOKEN"));
            if has_token {
                eprintln!("Session OK.");
                return;
            }
            eprintln!(
                "Login succeeded but session is not trusted (missing X-APPLE-WEBAUTH-TOKEN).\n\
                 Complete 2FA first: ICLOUD_USERNAME={username} KEI_DATA_DIR={} kei login get-code\n\
                 Or set ICLOUD_TEST_COOKIE_DIR to a directory with a trusted session.",
                cookie_dir.display()
            );
            std::process::exit(1);
        }

        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains(RATE_LIMIT_MARKER) {
            RATE_LIMITED.store(true, Ordering::SeqCst);
            eprintln!("\n*** ABORTING: Apple 503 rate limit during session validation ***");
            std::process::exit(1);
        }

        if stderr.contains("2FA") || stderr.contains("Two-factor") {
            eprintln!(
                "\n*** ABORTING: 2FA required but not available in test mode ***\n\
                 Complete 2FA first: ICLOUD_USERNAME={username} KEI_DATA_DIR={} kei login get-code\n\
                 Or set ICLOUD_TEST_COOKIE_DIR to a directory with a trusted session.",
                cookie_dir.display()
            );
            std::process::exit(1);
        }

        panic!("Session validation (login) failed — credentials may be invalid.\nstderr: {stderr}");
    });
}

/// Refresh the authentication session by running `kei login`.
///
/// Called reactively when a test command fails with an authentication error
/// mid-run (stale session). Panics if the refresh itself fails.
fn refresh_auth() {
    let (username, password, cookie_dir) = AUTH_CREDS
        .get()
        .expect("refresh_auth called before require_preauth");

    eprintln!("Running login to refresh session...");
    let output = assert_cmd::cargo_bin_cmd!("kei")
        .env("ICLOUD_USERNAME", username)
        .env("KEI_DATA_DIR", cookie_dir)
        .args(["login", "--password", password])
        .timeout(std::time::Duration::from_secs(90))
        .output()
        .expect("failed to run login");

    if output.status.success() {
        refresh_isolated_auth_material(username, cookie_dir);
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
/// refreshes the session via `kei login` and retries once. If the retry
/// also hits the same auth error, aborts the entire test suite.
///
/// Does **not** retry on 503 rate limits or other errors.
#[allow(dead_code)]
pub fn with_auth_retry(f: impl Fn()) {
    use std::panic::{AssertUnwindSafe, catch_unwind, resume_unwind};

    match catch_unwind(AssertUnwindSafe(&f)) {
        Ok(()) => {}
        Err(payload) => {
            let msg = payload
                .downcast_ref::<String>()
                .map(|s| s.as_str())
                .or_else(|| payload.downcast_ref::<&str>().copied())
                .unwrap_or("");

            if !is_retryable_auth_failure(msg) {
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

                    if is_retryable_auth_failure(retry_msg) {
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

/// Return true for stale-auth failures where a refreshed session plus a
/// whole-command retry is expected to recover.
#[allow(dead_code)]
pub(crate) fn is_retryable_auth_failure(msg: &str) -> bool {
    msg.contains(AUTH_FAILURE_MARKER) || msg.contains(CLOUDKIT_STALE_SESSION_MARKER)
}

/// Require a pre-authenticated session. Returns `(username, password, cookie_dir)`.
///
/// All tests share one pre-authenticated session source, then copy its auth
/// material into an isolated data directory so SQLite state does not leak
/// between tests. **Auth-requiring tests must run single-threaded:**
///
/// ```sh
/// cargo test --test sync -- --ignored --test-threads=1
/// ```
///
/// On the first call, runs `kei login` to validate (and refresh if needed)
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
    live_selection::ensure_selection(&username, &password, &dir);
    let isolated_dir = isolated_data_dir(&username, &dir);
    (username, password, isolated_dir)
}

/// Strip ANSI escape sequences from a string (for log assertions).
///
/// kei's `tracing_subscriber::fmt()` writer emits ANSI color codes even
/// when stderr is a pipe (not a TTY), which splits log fields like
/// `stage="scan_started"` with escape bytes around `=` and `"`. Stripping
/// before predicate evaluation is what makes `wait_for_stderr_line`'s
/// callers' substring checks robust.
#[allow(dead_code)]
pub fn strip_ansi(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut in_escape = false;
    for c in s.chars() {
        if c == '\x1b' {
            in_escape = true;
        } else if in_escape {
            if c.is_ascii_alphabetic() {
                in_escape = false;
            }
        } else {
            result.push(c);
        }
    }
    result
}

/// Stream a child's piped stderr on a worker thread until `predicate(line)`
/// returns true or the deadline elapses. Returns `Some(())` on a hit,
/// `None` on timeout / EOF.
///
/// `predicate` is invoked on the ANSI-stripped form of each line, so callers
/// can write plain substring checks against the rendered field text without
/// worrying about color codes from kei's `tracing_subscriber::fmt()` writer.
///
/// Uses a worker thread + mpsc channel so the wall-clock deadline is honored
/// even if the child stops emitting lines mid-stream (a bare `BufReader::lines()`
/// loop on the test thread would block forever in that case).
///
/// `child.stderr` must have been spawned with `Stdio::piped()`. This call
/// `take()`s it, so callers wanting both this sync point and full-output
/// capture must split the stream themselves.
#[allow(dead_code)]
pub fn wait_for_stderr_line(
    child: &mut std::process::Child,
    predicate: impl Fn(&str) -> bool + Send + 'static,
    deadline: std::time::Duration,
) -> Option<()> {
    use std::io::{BufRead, BufReader};
    use std::sync::mpsc;
    use std::thread;
    use std::time::Instant;

    let stderr = child.stderr.take()?;
    let (tx, rx) = mpsc::channel::<String>();
    thread::spawn(move || {
        let mut buffered = BufReader::new(stderr);
        let mut line = String::new();
        loop {
            line.clear();
            match buffered.read_line(&mut line) {
                Ok(0) => break,
                Ok(_) => {
                    if tx.send(line.clone()).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    let end = Instant::now() + deadline;
    while Instant::now() < end {
        let remaining = end.saturating_duration_since(Instant::now());
        match rx.recv_timeout(remaining) {
            Ok(line) => {
                if predicate(&strip_ansi(&line)) {
                    return Some(());
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout | mpsc::RecvTimeoutError::Disconnected) => {
                return None;
            }
        }
    }
    None
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

/// Quote a string for the simple TOML snippets live tests write at runtime.
#[allow(dead_code)]
pub fn toml_string(s: &str) -> String {
    let mut out = String::from("\"");
    for ch in s.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            _ => out.push(ch),
        }
    }
    out.push('"');
    out
}

/// Write a per-test TOML config next to the test data directory.
///
/// Live tests run single-threaded, so a stable filename per process keeps the
/// command lines short without racing another test in the same target.
#[allow(dead_code)]
pub fn write_toml_config(dir: &Path, name: &str, body: &str) -> PathBuf {
    std::fs::create_dir_all(dir).expect("create config dir");
    let path = dir.join(format!(".kei-{name}-{}.toml", std::process::id()));
    std::fs::write(&path, body).expect("write test TOML config");
    path
}
