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
pub fn creds_or_skip() -> Option<(String, String)> {
    dotenvy::dotenv().ok();
    let username = std::env::var("ICLOUD_USERNAME").ok()?;
    let password = std::env::var("ICLOUD_PASSWORD").ok()?;
    if username.is_empty() || password.is_empty() {
        return None;
    }
    Some((username, password))
}
