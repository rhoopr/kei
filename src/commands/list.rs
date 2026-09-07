#![allow(
    clippy::print_stdout,
    reason = "CLI subcommand whose primary purpose is to print album/library lists to stdout"
)]

use crate::auth;
use crate::cli;
use crate::config;
use crate::retry;
use std::collections::BTreeSet;

use super::service::{init_photos_service, resolve_libraries, retry_on_lock_contention};

/// Run the list command: list albums or libraries.
pub(crate) async fn run_list(
    what: cli::ListCommand,
    pw: &cli::PasswordArgs,
    cli_libraries: Vec<String>,
    globals: &config::GlobalArgs,
    toml: Option<&config::TomlConfig>,
    input_mode: crate::InputMode,
) -> anyhow::Result<()> {
    let (username, password, domain, cookie_directory) = config::resolve_auth(globals, pw, toml);

    if username.is_empty() {
        anyhow::bail!(
            "Set your iCloud username with ICLOUD_USERNAME or [auth].username before listing iCloud Photos."
        );
    }

    let password_provider = super::super::make_provider_from_auth(
        pw,
        password,
        &username,
        &cookie_directory,
        toml,
        input_mode,
    );

    let auth_result = retry_on_lock_contention(|| {
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

    let api_retry_config = retry::RetryConfig::default();
    // `kei list` has no friendly progress bar (it just prints names to stdout),
    // so recovery narration would have nothing to coexist with. Off-mode
    // preserves today's exact output for scripted consumers parsing the album
    // list.
    let (_shared_session, mut photos_service) = init_photos_service(
        auth_result,
        api_retry_config,
        crate::personality::Mode::Off,
        None,
    )
    .await?;

    match what {
        cli::ListCommand::Libraries => {
            let private: Vec<String> = {
                let private = photos_service.fetch_private_libraries().await?;
                private.keys().cloned().collect()
            };
            let shared: Vec<String> = {
                let shared = photos_service.fetch_shared_libraries().await?;
                shared.keys().cloned().collect()
            };
            print!("{}", format_libraries(private, shared));
        }
        cli::ListCommand::Albums => {
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

fn classify_library(zone_name: &str) -> Option<&'static str> {
    if zone_name == "PrimarySync" {
        Some("primary")
    } else if zone_name.starts_with("SharedSync-") {
        Some("shared")
    } else {
        None
    }
}

fn format_libraries(
    private: impl IntoIterator<Item = String>,
    shared: impl IntoIterator<Item = String>,
) -> String {
    let mut all: BTreeSet<String> = BTreeSet::new();
    all.extend(private);
    all.extend(shared);

    let mut ordered: Vec<String> = Vec::with_capacity(all.len());
    if all.contains("PrimarySync") {
        ordered.push("PrimarySync".to_string());
    }
    ordered.extend(all.into_iter().filter(|name| name != "PrimarySync"));

    let mut out = String::from("Libraries:\n");
    for zone_name in ordered {
        match classify_library(&zone_name) {
            Some(kind) => {
                out.push_str(&format!("  {zone_name} ({kind})\n"));
            }
            None => {
                out.push_str(&format!("  {zone_name}\n"));
            }
        }
    }
    out.push_str("\nUse these names in [filters].libraries, or use: primary, shared, all, none.\n");
    out
}

#[cfg(test)]
mod tests {
    use super::format_libraries;

    #[test]
    fn format_libraries_primary_then_shared_with_labels() {
        let rendered = format_libraries(
            ["SharedSync-AAAA", "PrimarySync"].map(str::to_string),
            ["SharedSync-BBBB"].map(str::to_string),
        );
        let lines: Vec<&str> = rendered.lines().collect();
        assert_eq!(lines[0], "Libraries:");
        assert_eq!(lines[1], "  PrimarySync (primary)");
        assert_eq!(lines[2], "  SharedSync-AAAA (shared)");
        assert_eq!(lines[3], "  SharedSync-BBBB (shared)");
        assert_eq!(
            lines[5],
            "Use these names in [filters].libraries, or use: primary, shared, all, none."
        );
    }

    #[test]
    fn format_libraries_dedupes_names_from_private_and_shared_sources() {
        let rendered = format_libraries(
            ["PrimarySync", "SharedSync-AAAA"].map(str::to_string),
            ["SharedSync-AAAA"].map(str::to_string),
        );
        assert_eq!(rendered.matches("SharedSync-AAAA").count(), 1);
    }

    #[test]
    fn format_libraries_unknown_zone_stays_copy_pasteable() {
        let rendered = format_libraries(["CustomZone".to_string()], Vec::new());
        assert!(rendered.contains("  CustomZone"));
        assert!(!rendered.contains("CustomZone ("));
    }
}
