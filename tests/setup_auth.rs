//! Pre-authentication fixture for integration tests.
//!
//! Establishes an iCloud session so auth-requiring tests can run without
//! interactive 2FA. Requires `ICLOUD_USERNAME` and `ICLOUD_PASSWORD` in `.env`.
//!
//! Setup (interactive — prompts for 2FA code):
//!
//! ```sh
//! env (cat .env | grep -v '^#') cargo run -- sync --auth-only --cookie-directory .test-cookies
//! ```
//!
//! Verify an existing session is still valid:
//!
//! ```sh
//! cargo test --test setup_auth -- --ignored
//! ```

mod common;

/// Verify that a pre-authenticated session exists and is usable.
#[test]
#[ignore]
fn verify_preauth_session() {
    let Some((username, password, cookie_dir)) = common::require_preauth() else {
        return;
    };

    // Try auth-only with the pre-auth cookies — should succeed without 2FA
    common::cmd()
        .args([
            "sync",
            "--auth-only",
            "--username",
            &username,
            "--password",
            &password,
            "--cookie-directory",
            cookie_dir.to_str().unwrap(),
        ])
        .timeout(std::time::Duration::from_secs(60))
        .assert()
        .success();

    eprintln!("Pre-auth session is valid.");
}
