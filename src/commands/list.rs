#![allow(
    clippy::print_stdout,
    reason = "CLI subcommand whose primary purpose is to print album/library lists to stdout"
)]

use crate::auth;
use crate::cli;
use crate::config;
use crate::retry;

use super::service::{init_photos_service, resolve_libraries, retry_on_lock_contention};

/// Run the list command: list albums or libraries.
pub(crate) async fn run_list(
    what: cli::ListCommand,
    pw: &cli::PasswordArgs,
    library: Option<String>,
    globals: &config::GlobalArgs,
    toml: Option<&config::TomlConfig>,
) -> anyhow::Result<()> {
    let (username, password, domain, cookie_directory) = config::resolve_auth(globals, pw, toml);

    if username.is_empty() {
        anyhow::bail!("--username is required");
    }

    let password_provider =
        super::super::make_provider_from_auth(pw, password, &username, &cookie_directory, toml);

    let auth_result = retry_on_lock_contention(|| {
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

    let api_retry_config = retry::RetryConfig::default();
    let (_shared_session, mut photos_service) =
        init_photos_service(auth_result, api_retry_config).await?;

    match what {
        cli::ListCommand::Libraries => {
            println!("Private libraries:");
            let private = photos_service.fetch_private_libraries().await?;
            for name in private.keys() {
                println!("  {name}");
            }
            println!("Shared libraries:");
            let shared = photos_service.fetch_shared_libraries().await?;
            for name in shared.keys() {
                println!("  {name}");
            }
        }
        cli::ListCommand::Albums => {
            let cli_libraries = library.into_iter().collect();
            let selector = config::resolve_library_selector(
                cli_libraries,
                toml.and_then(|t| t.filters.as_ref()),
            )?;
            let libraries = resolve_libraries(&selector, &mut photos_service).await?;
            for library in &libraries {
                println!("Library: {}", library.zone_name());
                let albums = library.albums().await?;
                for name in albums.keys() {
                    println!("  {name}");
                }
            }
        }
    }
    Ok(())
}
