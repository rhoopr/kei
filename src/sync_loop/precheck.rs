//! Scoped database change tokens and selected-zone watch prechecks.

use std::sync::Arc;

use anyhow::Context;

use crate::sync_cycle::LibraryState;
use crate::{config, download, state};

#[derive(Debug, PartialEq, Eq)]
pub(super) enum WatchPrecheck {
    SkipAll,
    Proceed {
        changed_zones: Option<rustc_hash::FxHashSet<String>>,
        db_sync_token_after_success: Option<String>,
    },
}

impl WatchPrecheck {
    pub(super) fn proceed_all() -> Self {
        Self::Proceed {
            changed_zones: None,
            db_sync_token_after_success: None,
        }
    }

    pub(super) fn changed_zones(&self) -> Option<&rustc_hash::FxHashSet<String>> {
        match self {
            Self::SkipAll => None,
            Self::Proceed { changed_zones, .. } => changed_zones.as_ref(),
        }
    }

    pub(super) fn db_sync_token_after_success(&self) -> Option<&str> {
        match self {
            Self::SkipAll => None,
            Self::Proceed {
                db_sync_token_after_success,
                ..
            } => db_sync_token_after_success.as_deref(),
        }
    }

    pub(super) fn should_sync_zone(&self, zone_name: &str) -> bool {
        match self {
            Self::SkipAll => false,
            Self::Proceed {
                changed_zones: Some(zones),
                ..
            } => zones.contains(zone_name),
            Self::Proceed {
                changed_zones: None,
                ..
            } => true,
        }
    }

    fn include_local_work_zones(&mut self, zones: rustc_hash::FxHashSet<String>) {
        if zones.is_empty() {
            return;
        }
        match self {
            Self::SkipAll => {
                *self = Self::Proceed {
                    changed_zones: Some(zones),
                    db_sync_token_after_success: None,
                };
            }
            Self::Proceed {
                changed_zones: Some(changed_zones),
                ..
            } => changed_zones.extend(zones),
            Self::Proceed {
                changed_zones: None,
                ..
            } => {}
        }
    }
}

pub(super) async fn include_pending_local_work(
    watch_precheck: &mut WatchPrecheck,
    db: &dyn download::DownloadStore,
    metadata: &config::MetadataConfig,
    library_states: &[LibraryState],
) {
    let rewrite_writers_enabled = download::metadata_rewrite::writers_enabled(metadata);
    let mut local_work_zones = rustc_hash::FxHashSet::default();
    for library in library_states {
        let zone = library.zone_name.as_str();
        let capture_pending = match db
            .has_metadata_capture_work(&[zone], state::METADATA_CAPTURE_REVISION)
            .await
        {
            Ok(pending) => pending,
            Err(error) => {
                tracing::warn!(error = %error, "Could not inspect metadata-capture work before watch pre-check");
                true
            }
        };
        let rewrite_pending = if rewrite_writers_enabled {
            match db
                .get_pending_metadata_rewrites_page(Some(&[zone]), 0, 1)
                .await
            {
                Ok(rows) => !rows.is_empty(),
                Err(error) => {
                    tracing::warn!(error = %error, "Could not inspect metadata-rewrite work before watch pre-check");
                    true
                }
            }
        } else {
            false
        };
        let identity_pending = match db.get_metadata(&state::unresolved_identity_key(zone)).await {
            Ok(marker) => marker.is_some(),
            Err(error) => {
                tracing::warn!(error = %error, "Could not inspect unresolved identity work before watch pre-check");
                true
            }
        };
        if capture_pending || rewrite_pending || identity_pending {
            local_work_zones.insert(library.zone_name.clone());
        }
    }
    if !local_work_zones.is_empty() {
        tracing::debug!(
            libraries = local_work_zones.len(),
            "Pending local work bypassed the watch no-change shortcut"
        );
        watch_precheck.include_local_work_zones(local_work_zones);
    }
}

/// Legacy metadata key for the unscoped database-level token used by
/// `/changes/database` before scoped provenance rows.
#[cfg(test)]
const DB_SYNC_TOKEN_KEY: &str = "db_sync_token";

const SCOPED_DB_SYNC_TOKEN_PROVIDER: &str = "icloud";

const SCOPED_DB_SYNC_TOKEN_SHAPE_VERSION: i64 = 1;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct DbPrecheckScope {
    provider: String,
    account: String,
    shape_version: i64,
    scope_hash: String,
    selected_zones_json: String,
    scope_json: String,
}

impl DbPrecheckScope {
    pub(super) fn from_config(
        config: &config::Config,
        library_states: &[LibraryState],
        build_download_config: &crate::sync_cycle::BuildDownloadConfigFn<'_>,
        enum_config_hash: &str,
    ) -> anyhow::Result<Self> {
        let mut selected_zones: Vec<String> =
            library_states.iter().map(|s| s.zone_name.clone()).collect();
        selected_zones.sort();

        let download_config_hash = build_download_config(
            download::SyncMode::Full,
            Arc::new(rustc_hash::FxHashSet::default()),
            Arc::new(download::AssetGroupings::default()),
            Arc::from(
                selected_zones
                    .first()
                    .map(String::as_str)
                    .unwrap_or(crate::icloud::photos::PRIMARY_ZONE_NAME),
            ),
        );
        let download_config_hash = download::hash_download_config(&download_config_hash);

        let selected_zones_json = serde_json::to_string(&selected_zones)
            .context("serialize scoped database token selected zones")?;
        let scope_json = download::sync_coverage_fingerprint_json(
            config,
            SCOPED_DB_SYNC_TOKEN_PROVIDER,
            SCOPED_DB_SYNC_TOKEN_SHAPE_VERSION,
            &selected_zones,
            enum_config_hash,
            &download_config_hash,
        )?;
        let scope_hash =
            hash_scoped_db_precheck_scope(SCOPED_DB_SYNC_TOKEN_SHAPE_VERSION, &scope_json);

        Ok(Self {
            provider: SCOPED_DB_SYNC_TOKEN_PROVIDER.to_string(),
            account: config.auth.username.clone(),
            shape_version: SCOPED_DB_SYNC_TOKEN_SHAPE_VERSION,
            scope_hash,
            selected_zones_json,
            scope_json,
        })
    }

    fn to_state_row(&self, token: &str) -> state::ScopedDbSyncToken {
        state::ScopedDbSyncToken {
            provider: self.provider.clone(),
            account: self.account.clone(),
            shape_version: self.shape_version,
            scope_hash: self.scope_hash.clone(),
            selected_zones_json: self.selected_zones_json.clone(),
            scope_json: self.scope_json.clone(),
            token: token.to_string(),
        }
    }
}

fn hash_scoped_db_precheck_scope(shape_version: i64, scope_json: &str) -> String {
    use sha2::{Digest, Sha256};
    use std::fmt::Write;

    let mut hasher = Sha256::new();
    hasher.update(shape_version.to_le_bytes());
    hasher.update(b"\0");
    hasher.update(scope_json.as_bytes());
    let hash = hasher.finalize();
    let mut hex = String::with_capacity(64);
    for b in hash {
        let _ = Write::write_fmt(&mut hex, format_args!("{b:02x}"));
    }
    hex
}

pub(super) async fn store_scoped_db_sync_token(
    db: &dyn state::SyncTokenStore,
    scope: &DbPrecheckScope,
    token: &str,
) {
    if let Err(e) = db
        .upsert_scoped_db_sync_token(scope.to_state_row(token))
        .await
    {
        tracing::warn!(error = %e, "Failed to store scoped db sync token");
    }
}

/// Check `changes/database` to determine if this watch cycle can be skipped.
///
/// Returns `SkipAll` when a complete pre-check reports no selected-zone changes.
/// An empty complete page still skips the cycle but keeps the previous
/// scoped DB token, so the next watch wakeup rechecks from the same point.
pub(super) async fn check_changes_database(
    state_db: Option<&dyn state::SyncTokenStore>,
    library_states: &[LibraryState],
    photos_service: &mut crate::icloud::photos::PhotosService,
    scope: &DbPrecheckScope,
) -> WatchPrecheck {
    let Some(db) = state_db else {
        return WatchPrecheck::proceed_all();
    };
    if library_states.is_empty() {
        return WatchPrecheck::SkipAll;
    }
    let scoped_token = match db
        .get_scoped_db_sync_token(
            &scope.provider,
            &scope.account,
            scope.shape_version,
            &scope.scope_hash,
        )
        .await
    {
        Ok(Some(token)) if !token.token.trim().is_empty() => token,
        Ok(_) => {
            return match photos_service.changes_database(None).await {
                Ok(db_resp) if !db_resp.more_coming => WatchPrecheck::Proceed {
                    changed_zones: None,
                    db_sync_token_after_success: Some(db_resp.sync_token),
                },
                Ok(db_resp) => {
                    tracing::debug!(
                        zones = db_resp.zones.len(),
                        "changes/database bootstrap had more pages; scoped db sync token not stored"
                    );
                    WatchPrecheck::proceed_all()
                }
                Err(e) => {
                    tracing::debug!(
                        error = %e,
                        "changes/database bootstrap failed; proceeding with sync"
                    );
                    WatchPrecheck::proceed_all()
                }
            };
        }
        Err(e) => {
            tracing::warn!(
                error = %e,
                scope_hash = %scope.scope_hash,
                "Failed to read scoped changes/database sync token; proceeding with sync"
            );
            return WatchPrecheck::proceed_all();
        }
    };
    if serde_json::from_str::<serde_json::Value>(&scoped_token.scope_json).is_err() {
        tracing::debug!(
            scope_hash = %scope.scope_hash,
            "Stored scoped changes/database scope JSON is invalid; proceeding with sync"
        );
        return WatchPrecheck::proceed_all();
    }
    if scoped_token.scope_json != scope.scope_json {
        tracing::debug!(
            scope_hash = %scope.scope_hash,
            "Stored scoped changes/database scope JSON mismatch; proceeding with sync"
        );
        return WatchPrecheck::proceed_all();
    }
    if serde_json::from_str::<Vec<String>>(&scoped_token.selected_zones_json).is_err() {
        tracing::debug!(
            scope_hash = %scope.scope_hash,
            "Stored scoped changes/database selected-zone JSON is invalid; proceeding with sync"
        );
        return WatchPrecheck::proceed_all();
    }
    if scoped_token.selected_zones_json != scope.selected_zones_json {
        tracing::debug!(
            scope_hash = %scope.scope_hash,
            "Stored scoped changes/database selected zones mismatch; proceeding with sync"
        );
        return WatchPrecheck::proceed_all();
    }

    match photos_service
        .changes_database(Some(scoped_token.token.as_str()))
        .await
    {
        Ok(db_resp) => {
            let selected_zones: rustc_hash::FxHashSet<&str> = library_states
                .iter()
                .map(|s| s.zone_name.as_str())
                .collect();
            let mut changed_selected_zones = rustc_hash::FxHashSet::default();
            let has_any_changed_zone = !db_resp.zones.is_empty();
            if db_resp.more_coming {
                tracing::debug!("changes/database has more pages (moreComing=true)");
            }
            for z in &db_resp.zones {
                tracing::debug!(
                    zone = %z.zone_id.zone_name,
                    zone_sync_token = %z.sync_token,
                    "changes/database: zone has changes"
                );
                if selected_zones.contains(z.zone_id.zone_name.as_str()) {
                    changed_selected_zones.insert(z.zone_id.zone_name.clone());
                }
            }

            if changed_selected_zones.is_empty() {
                if db_resp.more_coming {
                    return WatchPrecheck::Proceed {
                        changed_zones: None,
                        db_sync_token_after_success: Some(db_resp.sync_token),
                    };
                }
                if has_any_changed_zone {
                    store_scoped_db_sync_token(db, scope, &db_resp.sync_token).await;
                } else {
                    tracing::debug!(
                        "changes/database returned an empty complete page; skipping without advancing scoped db sync token"
                    );
                }
                tracing::info!(
                    "No selected library changes detected (changes/database), skipping cycle"
                );
                return WatchPrecheck::SkipAll;
            }

            WatchPrecheck::Proceed {
                changed_zones: if db_resp.more_coming {
                    None
                } else {
                    Some(changed_selected_zones)
                },
                db_sync_token_after_success: Some(db_resp.sync_token),
            }
        }
        Err(e) => {
            tracing::debug!(
                error = %e,
                "changes/database pre-check failed, proceeding with sync"
            );
            WatchPrecheck::proceed_all()
        }
    }
}

#[cfg(test)]
mod tests;
