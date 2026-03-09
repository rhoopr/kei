use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;

/// Set when any test detects an Apple 503 rate-limit response.
static RATE_LIMITED: AtomicBool = AtomicBool::new(false);

const RATE_LIMIT_MARKER: &str = "503 Service Temporarily Unavailable";

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

/// Build an `assert_cmd::Command` for the icloudpd-rs binary.
///
/// Loads `.env` from the repo root (if present) so that `ICLOUD_USERNAME`
/// and `ICLOUD_PASSWORD` are available to the child process.
pub fn cmd() -> assert_cmd::Command {
    init_env();
    assert_cmd::cargo_bin_cmd!("icloudpd-rs")
}

/// Skip the current test when credentials are not configured.
///
/// Returns `(username, password)` if both `ICLOUD_USERNAME` and
/// `ICLOUD_PASSWORD` are set, otherwise prints a message and returns `None`.
#[allow(dead_code)]
pub fn creds_or_skip() -> Option<(String, String)> {
    init_env();
    let username = std::env::var("ICLOUD_USERNAME").ok()?;
    let password = std::env::var("ICLOUD_PASSWORD").ok()?;
    if username.is_empty() || password.is_empty() {
        return None;
    }
    Some((username, password))
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

/// Require a pre-authenticated session. Returns `(username, password, cookie_dir)`.
///
/// All tests share the same cookie directory so only one Apple API session
/// is used per test run. **Auth-requiring tests must run single-threaded:**
///
/// ```sh
/// cargo test --test sync -- --test-threads=1
/// ```
///
/// Panics (aborting the test) if:
/// - Credentials are not configured
/// - The shared cookie directory does not contain session files
///
/// Pre-auth setup (fish shell — use `$(…)` instead of `(…)` in bash/zsh):
///
/// ```sh
/// env (cat .env | grep -v '^#') cargo run -- sync --auth-only --cookie-directory .test-cookies
/// ```
#[allow(dead_code)]
pub fn require_preauth() -> (String, String, PathBuf) {
    if RATE_LIMITED.load(Ordering::SeqCst) {
        eprintln!("\n*** ABORTING: Apple 503 rate limit detected in earlier test ***");
        std::process::exit(1);
    }
    let (username, password) = creds_or_skip().expect(
        "AUTH TESTS REQUIRE ICLOUD_USERNAME and ICLOUD_PASSWORD — set them in .env or environment",
    );
    let dir = cookie_dir();
    let sanitized: String = username
        .chars()
        .filter(|c| c.is_alphanumeric() || *c == '_')
        .collect();
    let session_file = dir.join(format!("{sanitized}.session"));
    assert!(
        session_file.exists(),
        "Pre-auth session not found at {}. Run pre-auth setup first (see tests/setup_auth.rs).",
        session_file.display()
    );
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
