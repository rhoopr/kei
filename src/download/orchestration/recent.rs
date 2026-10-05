//! Durable, conservative recovery for bounded recent selection.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::config::{DownloadConfig, hash_download_config};

#[derive(Debug, thiserror::Error)]
#[error("Recent selection recovery did not complete safely")]
pub(super) struct RecentSelectionError(#[source] pub(super) anyhow::Error);

#[derive(Serialize, Deserialize)]
struct Receipt {
    version: u8,
    fingerprint: String,
    cursor_hash: String,
    bounded: bool,
    complete: bool,
}

fn key(config: &DownloadConfig) -> String {
    format!("recent_selection_recovery:{}", config.library)
}

fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn fingerprint(passes: &[crate::commands::AlbumPass], config: &DownloadConfig) -> Result<String> {
    let scopes: Vec<_> = passes
        .iter()
        .map(|pass| {
            let mut excludes: Vec<_> = pass.exclude_ids.iter().collect();
            excludes.sort_unstable();
            serde_json::json!({
                "kind": format!("{:?}", pass.kind),
                "provider_scope": pass.album.selection_scope(),
                "excludes": excludes,
            })
        })
        .collect();
    Ok(digest(&serde_json::to_vec(&serde_json::json!({
        "version": 1,
        "library": config.library.as_ref(),
        "download_config": hash_download_config(config),
        "enum_config": config.enum_config_hash.as_deref(),
        "before": config.skip_created_before.map(crate::config::CreatedDateFilter::fingerprint),
        "after": config.skip_created_after.map(crate::config::CreatedDateFilter::fingerprint),
        "scopes": scopes,
    }))?))
}

async fn read(config: &DownloadConfig) -> Result<Option<String>> {
    match &config.state_db {
        Some(db) => db
            .get_metadata(&key(config))
            .await
            .context("Read recent selection recovery"),
        None => Ok(None),
    }
}

pub(super) async fn active(config: &DownloadConfig) -> Result<bool> {
    if config.recent.is_some() {
        return Ok(true);
    }
    Ok(read(config).await?.is_some_and(|value| {
        serde_json::from_str::<Receipt>(&value).map_or(true, |receipt| {
            receipt.version != 1 || receipt.bounded || !receipt.complete
        })
    }))
}

pub(super) async fn needs_inventory(
    passes: &[crate::commands::AlbumPass],
    config: &DownloadConfig,
    prior_cursor: &str,
    changed: bool,
) -> Result<bool> {
    if changed {
        return Ok(true);
    }
    let Some(value) = read(config).await? else {
        return Ok(true);
    };
    let Ok(receipt) = serde_json::from_str::<Receipt>(&value) else {
        return Ok(true);
    };
    Ok(receipt.version != 1
        || !receipt.complete
        || receipt.fingerprint != fingerprint(passes, config)?
        || receipt.cursor_hash != digest(prior_cursor.as_bytes()))
}

/// Inspect unresolved or changed recent selection before an unchanged watch
/// precheck can skip the normal source owner.
pub(crate) async fn requires_recovery(
    passes: &[crate::commands::AlbumPass],
    config: &DownloadConfig,
    source_cursor_key: &str,
) -> Result<bool> {
    if !active(config).await? {
        return Ok(false);
    }
    let cursor = match &config.state_db {
        Some(db) => db.get_metadata(source_cursor_key).await?,
        None => None,
    };
    needs_inventory(passes, config, cursor.as_deref().unwrap_or(""), false).await
}

/// A fixed-size receipt is current only after its recorded successor becomes
/// the requested source cursor. Failed/cancelled checkpoint commits therefore
/// cannot turn an early receipt write into permission to skip recovery.
pub(super) async fn record(
    passes: &[crate::commands::AlbumPass],
    config: &DownloadConfig,
    cursor: &str,
    complete: bool,
) -> Result<()> {
    let Some(db) = &config.state_db else {
        return Ok(());
    };
    let receipt = Receipt {
        version: 1,
        fingerprint: fingerprint(passes, config)?,
        cursor_hash: digest(cursor.as_bytes()),
        bounded: config.recent.is_some(),
        complete,
    };
    db.set_metadata(&key(config), &serde_json::to_string(&receipt)?)
        .await
        .context("Persist recent selection recovery")
}

#[cfg(test)]
mod tests;
