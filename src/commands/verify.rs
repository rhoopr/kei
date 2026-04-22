#![allow(
    clippy::print_stdout,
    reason = "CLI subcommand whose primary purpose is to print verification results to stdout"
)]

use std::path::Path;

use crate::cli;
use crate::config;
use crate::download;
use crate::state;
use crate::state::StateDb;

/// Run the verify command.
pub(crate) async fn run_verify(
    args: cli::VerifyArgs,
    globals: &config::GlobalArgs,
    toml: Option<&config::TomlConfig>,
) -> anyhow::Result<()> {
    let db_path = super::super::get_db_path(globals, toml)?;

    if !db_path.exists() {
        println!("No state database found at {}", db_path.display());
        println!("Run a sync first to create the database.");
        return Ok(());
    }

    let db = state::SqliteStateDb::open(&db_path).await?;
    let summary = db.get_summary().await?;

    println!("Verifying {} downloaded assets...", summary.downloaded);
    println!();

    let mut missing = 0u64;
    let mut corrupted = 0u64;
    let mut verified = 0u64;

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

            if let Some(local_path) = &asset.local_path {
                if !local_path.exists() {
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
                    missing += 1;
                    continue;
                }

                if args.checksums {
                    if let Some(local_cksum) = &asset.local_checksum {
                        match verify_local_checksum(local_path, local_cksum).await {
                            Ok(true) => verified += 1,
                            Ok(false) => {
                                println!("CORRUPTED: {} ({})", local_path.display(), asset.id);
                                corrupted += 1;
                            }
                            Err(e) => {
                                println!("ERROR: {} - {}", local_path.display(), e);
                                corrupted += 1;
                            }
                        }
                    } else {
                        tracing::debug!(
                            id = %asset.id,
                            "No local checksum stored, skipping verification"
                        );
                        verified += 1;
                    }
                } else {
                    verified += 1;
                }
            } else {
                println!("NO PATH: {} - no local path recorded", asset.id);
                missing += 1;
            }
        }
    }

    println!();
    println!("Results:");
    println!("  Verified:  {verified}");
    println!("  Missing:   {missing}");
    if args.checksums {
        println!("  Corrupted: {corrupted}");
    }

    if missing > 0 || corrupted > 0 {
        anyhow::bail!("verify failed: {missing} missing, {corrupted} corrupted");
    }

    Ok(())
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

        assert!(!verify_local_checksum(
            &file_path,
            "0000000000000000000000000000000000000000000000000000000000000000"
        )
        .await
        .unwrap());
    }

    #[tokio::test]
    async fn verify_local_checksum_nonexistent_file_errors() {
        let dir = tempfile::tempdir().unwrap();
        let result = verify_local_checksum(&dir.path().join("nonexistent_file.bin"), "abcd").await;
        assert!(result.is_err());
    }
}
