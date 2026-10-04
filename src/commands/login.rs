#![allow(
    clippy::print_stdout,
    reason = "CLI subcommand whose primary purpose is to print login/2FA status to stdout"
)]

use crate::auth;
use crate::cli;
use crate::config;

use super::service::retry_on_lock_contention;

/// Run the login command: authenticate, request 2FA push, or submit a 2FA code.
pub(crate) async fn run_login(
    subcommand: Option<cli::LoginCommand>,
    pw: &cli::PasswordArgs,
    globals: &config::GlobalArgs,
    toml: Option<&config::TomlConfig>,
    input_mode: crate::InputMode,
) -> anyhow::Result<()> {
    let (username, password, domain, cookie_directory) = config::resolve_auth(globals, pw, toml);

    if username.is_empty() {
        anyhow::bail!(
            "Set your iCloud username with ICLOUD_USERNAME or [auth].username before logging in."
        );
    }

    let password_provider = super::super::make_provider_from_auth(
        pw,
        password,
        &username,
        &cookie_directory,
        domain.as_str(),
        toml,
        input_mode,
    );

    match subcommand {
        Some(cli::LoginCommand::GetCode) => {
            retry_on_lock_contention(|| {
                auth::send_2fa_push(
                    &cookie_directory,
                    &username,
                    &password_provider,
                    domain.as_str(),
                )
            })
            .await?;
            println!("2FA code requested. Check your trusted devices, then run:");
            println!("  kei login submit-code <CODE>");
        }
        Some(cli::LoginCommand::SubmitCode { code }) => {
            let result = retry_on_lock_contention(|| {
                auth::authenticate_in_input_mode(
                    &cookie_directory,
                    &username,
                    &password_provider,
                    domain.as_str(),
                    None,
                    None,
                    Some(&code),
                    input_mode,
                )
            })
            .await?;
            if result.requires_2fa {
                println!("2FA code accepted. Session is now authenticated.");
            } else {
                println!("Session is already authenticated.");
            }
        }
        None => {
            // Bare "kei login" = auth-only
            retry_on_lock_contention(|| {
                auth::authenticate_in_input_mode(
                    &cookie_directory,
                    &username,
                    &password_provider,
                    domain.as_str(),
                    None,
                    None,
                    None,
                    input_mode,
                )
            })
            .await?;
            println!("Authentication completed successfully.");
        }
    }
    Ok(())
}
