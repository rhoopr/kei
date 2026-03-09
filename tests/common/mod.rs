use std::path::PathBuf;

/// Build an `assert_cmd::Command` for the icloudpd-rs binary.
///
/// Loads `.env` from the repo root (if present) so that `ICLOUD_USERNAME`
/// and `ICLOUD_PASSWORD` are available to the child process.
#[allow(deprecated)]
pub fn cmd() -> assert_cmd::Command {
    dotenvy::dotenv().ok();
    assert_cmd::Command::cargo_bin("icloudpd-rs").expect("binary icloudpd-rs not found")
}

/// Skip the current test when credentials are not configured.
///
/// Returns `(username, password)` if both `ICLOUD_USERNAME` and
/// `ICLOUD_PASSWORD` are set, otherwise prints a message and returns `None`.
#[allow(dead_code)]
pub fn creds_or_skip() -> Option<(String, String)> {
    dotenvy::dotenv().ok();
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
    dotenvy::dotenv().ok();
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
/// Pre-auth setup:
///
/// ```sh
/// env (cat .env | grep -v '^#') cargo run -- sync --auth-only --cookie-directory .test-cookies
/// ```
#[allow(dead_code)]
pub fn require_preauth() -> (String, String, PathBuf) {
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
