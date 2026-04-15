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
) -> anyhow::Result<()> {
    let (username, password, domain, cookie_directory) = config::resolve_auth(globals, pw, toml);

    if username.is_empty() {
        anyhow::bail!("--username is required for login");
    }

    let password_provider =
        super::super::make_provider_from_auth(pw, password, &username, &cookie_directory, toml);

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
                auth::authenticate(
                    &cookie_directory,
                    &username,
                    &password_provider,
                    domain.as_str(),
                    None,
                    None,
                    Some(&code),
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
                auth::authenticate(
                    &cookie_directory,
                    &username,
                    &password_provider,
                    domain.as_str(),
                    None,
                    None,
                    None,
                )
            })
            .await?;
            println!("Authentication completed successfully.");
        }
    }
    Ok(())
}
