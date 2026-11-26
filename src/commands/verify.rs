#![allow(
    clippy::print_stdout,
    reason = "CLI subcommand whose primary purpose is to print verification results to stdout"
)]

use crate::state::ReportStateStore;
use std::path::Path;

use crate::cli;
use crate::config;
use crate::download;
use crate::state;

use super::{LISTING_CAP, print_truncation_tail};

/// Run the verify command.
pub(crate) async fn run_verify(
    args: cli::VerifyArgs,
    globals: &config::GlobalArgs,
    toml: Option<&config::TomlConfig>,
) -> anyhow::Result<()> {
    let db_path = super::super::get_db_path(globals, toml).await?;

    if !db_path.exists() {
        println!("No state database found at {}", db_path.display());
        println!("Run a sync first to create the database.");
        return Ok(());
    }

    let db = state::SqliteStateDb::open_owned_with_config(
        &db_path,
        &super::super::get_account_owner(globals, toml),
        &globals.config_source,
    )
    .await?;
    let summary = db.get_summary().await?;

    println!("Verifying {} downloaded assets...", summary.downloaded);
    println!();

    let mut missing: usize = 0;
    let mut checksum_verified: usize = 0;
    let mut present_unchecked: usize = 0;
    let mut different: usize = 0;
    let mut unreadable: usize = 0;
    let mut printed_issues: usize = 0;

    const PAGE_SIZE: u32 = 1000;
    let mut offset = 0u64;
    loop {
        let page = db.get_downloaded_page(offset, PAGE_SIZE).await?;
        if page.is_empty() {
            break;
        }
        offset += page.len() as u64;

        for asset in &page {
            debug_assert_eq!(asset.status, state::AssetStatus::Downloaded);

            let Some(local_path) = &asset.local_path else {
                if printed_issues < LISTING_CAP {
                    println!("MISSING: {} - no local path recorded", asset.id);
                    printed_issues += 1;
                }
                missing += 1;
                continue;
            };

            match inspect_local_path(local_path).await {
                LocalPathState::File => {}
                LocalPathState::Missing => {
                    if printed_issues < LISTING_CAP {
                        let downloaded_at = asset.downloaded_at.map_or_else(
                            || "unknown".to_string(),
                            |dt| dt.format("%Y-%m-%d").to_string(),
                        );
                        println!(
                            "MISSING: {} ({}) - downloaded {}",
                            local_path.display(),
                            asset.id,
                            downloaded_at
                        );
                        printed_issues += 1;
                    }
                    missing += 1;
                    continue;
                }
                LocalPathState::Unreadable(error) => {
                    if printed_issues < LISTING_CAP {
                        println!(
                            "UNREADABLE: {} ({}) - {}",
                            local_path.display(),
                            asset.id,
                            error
                        );
                        printed_issues += 1;
                    }
                    unreadable += 1;
                    continue;
                }
            }

            if !args.checksums {
                present_unchecked += 1;
                continue;
            }

            let Some(local_checksum) = &asset.local_checksum else {
                present_unchecked += 1;
                continue;
            };

            match verify_local_checksum(local_path, local_checksum).await {
                Ok(true) => checksum_verified += 1,
                Ok(false) => {
                    if printed_issues < LISTING_CAP {
                        println!(
                            "DIFFERENT: {} ({}) - may be an intentional edit",
                            local_path.display(),
                            asset.id
                        );
                        printed_issues += 1;
                    }
                    different += 1;
                }
                Err(e) => {
                    if printed_issues < LISTING_CAP {
                        println!(
                            "UNREADABLE: {} ({}) - {}",
                            local_path.display(),
                            asset.id,
                            e
                        );
                        printed_issues += 1;
                    }
                    unreadable += 1;
                }
            }
        }
    }

    let total_issues = missing + different + unreadable;
    if total_issues > printed_issues {
        println!();
        print_truncation_tail(total_issues, printed_issues);
    }

    println!();
    println!("Results:");
    if args.checksums {
        println!("  Checksum verified:              {checksum_verified}");
        println!("  Present, no checksum recorded:  {present_unchecked}");
        println!("  Different checksum:             {different}");
        println!("  Unreadable:                     {unreadable}");
        println!("  Missing:                        {missing}");
        println!();
        println!(
            "Scope: compared local files with their recorded SHA-256 checksums where available. A file without a recorded checksum is present but unchecked, not verified. A different checksum may be an intentional local edit. This check does not establish cloud completeness."
        );
    } else {
        println!("  Present, not checksum-verified: {present_unchecked}");
        println!("  Unreadable:                     {unreadable}");
        println!("  Missing:                        {missing}");
        println!();
        println!(
            "Scope: checked recorded local paths only. Checksums were not compared. Run `{}` to compare recorded SHA-256 values; files without a recorded checksum remain unchecked.",
            cli::command_for_config("verify --checksums", globals.config_source.recovery_path())
        );
    }

    if missing > 0 || different > 0 || unreadable > 0 {
        anyhow::bail!(
            "Verification found {missing} missing files, {different} files with a different checksum, and {unreadable} unreadable paths. Use `{}` to inspect an exact managed destination.",
            cli::command_for_config(
                "reconcile --inspect-path PATH",
                globals.config_source.recovery_path()
            )
        );
    }

    Ok(())
}

/// Classify a recorded local path without treating a stat failure as absence.
enum LocalPathState {
    File,
    Missing,
    Unreadable(std::io::Error),
}

async fn inspect_local_path(path: &Path) -> LocalPathState {
    match tokio::fs::metadata(path).await {
        Ok(metadata) if metadata.is_file() => LocalPathState::File,
        Ok(_) => LocalPathState::Unreadable(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "not a regular file",
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => LocalPathState::Missing,
        Err(error) => LocalPathState::Unreadable(error),
    }
}

/// Verify a file's SHA-256 hash against a hex-encoded expected value.
async fn verify_local_checksum(path: &Path, expected_hex: &str) -> anyhow::Result<bool> {
    let actual = download::file::compute_sha256(path).await?;
    Ok(actual == expected_hex)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn verify_local_checksum_match() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("local_match.bin");
        let content = b"hello world";
        std::fs::write(&file_path, content).unwrap();

        let hash = download::file::compute_sha256(&file_path).await.unwrap();
        assert!(verify_local_checksum(&file_path, &hash).await.unwrap());
    }

    #[tokio::test]
    async fn verify_local_checksum_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("local_mismatch.bin");
        std::fs::write(&file_path, b"hello world").unwrap();

        assert!(
            !verify_local_checksum(
                &file_path,
                "0000000000000000000000000000000000000000000000000000000000000000"
            )
            .await
            .unwrap()
        );
    }

    #[tokio::test]
    async fn verify_local_checksum_nonexistent_file_errors() {
        let dir = tempfile::tempdir().unwrap();
        let result = verify_local_checksum(&dir.path().join("nonexistent_file.bin"), "abcd").await;
        assert!(result.is_err());
    }
}
