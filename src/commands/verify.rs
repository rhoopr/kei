#![allow(
    clippy::print_stdout,
    reason = "CLI subcommand whose primary purpose is to print verification results to stdout"
)]

use std::path::Path;

use anyhow::Context;

use crate::cli;
use crate::config;
use crate::download;
use crate::sync_core::state::FileState;

use super::{LISTING_CAP, print_truncation_tail};

/// Run the verify command.
pub(crate) async fn run_verify(
    args: cli::VerifyArgs,
    globals: &config::GlobalArgs,
    toml: Option<&config::TomlConfig>,
) -> anyhow::Result<()> {
    let (location, state) = super::core_state(globals, toml).await?;
    let Some(state) = state else {
        println!("No state database found at {}", location.current.display());
        println!("Run a sync first to create the database.");
        return Ok(());
    };
    let directory = super::download_directory(toml)
        .context("Set [download].directory in the config file to verify downloaded files.")?;
    let mut targets: Vec<(String, String, Option<String>)> = state
        .list_files(&[FileState::Done])
        .await?
        .into_iter()
        .map(|file| (file.path, file.asset, file.sha256))
        .collect();
    targets.extend(
        state
            .list_imported()
            .await?
            .into_iter()
            .map(|copy| (copy.path, copy.master, copy.sha256)),
    );

    println!("Verifying {} downloaded assets...", targets.len());
    println!();

    let mut missing: usize = 0;
    let mut checksum_verified: usize = 0;
    let mut present_unchecked: usize = 0;
    let mut different: usize = 0;
    let mut unreadable: usize = 0;
    let mut printed_issues: usize = 0;
    let mut report = |line: String| {
        if printed_issues < LISTING_CAP {
            println!("{line}");
            printed_issues += 1;
        }
    };

    for (relative, asset, sha256) in &targets {
        let path = directory.join(relative);
        match inspect_local_path(&path).await {
            LocalPathState::File => {}
            LocalPathState::Missing => {
                report(format!("MISSING: {} ({asset})", path.display()));
                missing += 1;
                continue;
            }
            LocalPathState::Unreadable(error) => {
                report(format!(
                    "UNREADABLE: {} ({asset}) - {error}",
                    path.display()
                ));
                unreadable += 1;
                continue;
            }
        }
        if !args.checksums {
            present_unchecked += 1;
            continue;
        }
        let Some(expected) = sha256 else {
            present_unchecked += 1;
            continue;
        };
        match verify_local_checksum(&path, expected).await {
            Ok(true) => checksum_verified += 1,
            Ok(false) => {
                report(format!(
                    "DIFFERENT: {} ({asset}) - checksum does not match",
                    path.display()
                ));
                different += 1;
            }
            Err(error) => {
                report(format!(
                    "UNREADABLE: {} ({asset}) - {error}",
                    path.display()
                ));
                unreadable += 1;
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
            "Verification found {missing} missing files, {different} files with a different checksum, and {unreadable} unreadable paths. Run `{}` to download missing files again, or `{}` to inspect one file.",
            cli::command_for_config("reconcile", globals.config_source.recovery_path()),
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
