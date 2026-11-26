//! Provider capture composition; no selection or checkpoint policy lives here.

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use serde_json::{Value, json};

use crate::state::SqliteStateDb;
use crate::state::db::{
    account::AccountOwner,
    provider_inbox::{CapturedPageId, ObservedPage},
};

/// Operational safety bounds. Unfinished observations are never evicted to fit.
pub(crate) const MAX_CHANGES_PAGE_BYTES: usize = 16 * 1024 * 1024;
const MAX_SHADOW_INBOX_BYTES: u64 = 512 * 1024 * 1024;
const MAX_CATALOG_INDEX_BYTES: u64 = 512 * 1024 * 1024;

#[derive(Clone)]
pub(crate) struct ShadowCapture {
    pub(crate) db: Arc<SqliteStateDb>,
    pub(crate) owner: AccountOwner,
    realm: Arc<str>,
    pub(crate) capacity: u64,
    pub(crate) catalog_capacity: u64,
    recheck_after: Arc<AtomicI64>,
}

impl ShadowCapture {
    pub(crate) fn new(db: Arc<SqliteStateDb>, owner: AccountOwner, realm: &str) -> Self {
        Self {
            db,
            owner,
            realm: Arc::from(realm),
            capacity: MAX_SHADOW_INBOX_BYTES,
            catalog_capacity: MAX_CATALOG_INDEX_BYTES,
            recheck_after: Arc::default(),
        }
    }

    pub(crate) fn scope(&self, database: &str, zone: &Value) -> anyhow::Result<String> {
        anyhow::ensure!(
            matches!(database, "private" | "shared"),
            "Invalid provider database scope"
        );
        Ok(serde_json::to_string(&json!({
            "format": 1, "realm": &*self.realm, "container": "com.apple.photos.cloud",
            "environment": "production", "database": database,
            "zone": {"zoneName": zone.get("zoneName"), "ownerRecordName": zone.get("ownerRecordName")},
        }))?)
    }

    async fn project(
        &self,
        source: crate::state::db::provider_inbox::StoredPage,
    ) -> anyhow::Result<()> {
        let capture = self.clone();
        let plan =
            match tokio::task::spawn_blocking(move || super::projection::plan(&capture, source))
                .await?
            {
                Ok(plan) => plan,
                Err(error) => {
                    if super::error::classify_catalog_projection_error(&error) {
                        crate::support::record_storage(
                            crate::support::StorageComponent::Catalogue,
                            crate::support::StorageOutcome::SingleItemLimit,
                            None,
                        );
                    }
                    return Err(super::error::ShadowPageError::from(error).into());
                }
            };
        self.db
            .project_catalog_page(self.owner.clone(), plan, self.catalog_capacity)
            .await
            .map_err(|error| super::error::ShadowPageError::from(anyhow::Error::new(error)))?;
        Ok(())
    }

    pub(crate) async fn replay_catalog(
        &self,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> anyhow::Result<()> {
        while !cancel.is_cancelled() {
            let source = self
                .db
                .next_catalog_source(self.owner.clone(), MAX_CHANGES_PAGE_BYTES)
                .await
                .map_err(|error| super::error::ShadowPageError::from(anyhow::Error::new(error)))?;
            let Some(source) = source else {
                break;
            };
            self.project(source).await?;
        }
        if !cancel.is_cancelled() {
            self.reclaim_empty_pages().await?;
        }
        Ok(())
    }

    async fn reclaim_empty_pages(&self) -> anyhow::Result<()> {
        self.db
            .reclaim_empty_catalog_pages(self.owner.clone(), MAX_CHANGES_PAGE_BYTES)
            .await
            .map_err(|error| super::error::ShadowPageError::from(anyhow::Error::new(error)).into())
    }

    async fn recheck_next(&self, after: i64) -> anyhow::Result<Option<i64>> {
        let candidate = self
            .db
            .next_handoff_candidate(self.owner.clone(), after, MAX_CHANGES_PAGE_BYTES)
            .await
            .map_err(|error| super::error::ShadowPageError::from(anyhow::Error::new(error)))?;
        let Some(source) = candidate else {
            return Ok(None);
        };
        let id = source.id.0;
        self.project(source).await?;
        Ok(Some(id))
    }

    pub(crate) async fn capture(
        &self,
        page: ObservedPage,
    ) -> anyhow::Result<Option<CapturedPageId>> {
        let after = self.recheck_after.load(Ordering::Relaxed);
        self.recheck_after.store(
            self.recheck_next(after).await?.unwrap_or(0),
            Ordering::Relaxed,
        );
        if page.identities.is_empty() {
            let realm = self.realm.clone();
            let owner = self.owner.clone();
            self.db
                .validate_empty_poll(owner, page, realm)
                .await
                .map_err(|error| super::error::ShadowPageError::from(anyhow::Error::new(error)))?;
            return Ok(None);
        }
        let page = Arc::new(page);
        let capture = |final_attempt| {
            self.db.attempt_shadow_capture(
                self.owner.clone(),
                Arc::clone(&page),
                self.capacity,
                final_attempt,
            )
        };
        let id = match capture(false).await {
            Err(crate::state::error::StateError::ProviderInboxFull) => {
                self.reclaim_empty_pages().await?;
                let mut after = 0;
                while let Some(id) = self.recheck_next(after).await? {
                    after = id;
                }
                capture(true).await
            }
            result => result,
        }
        .map_err(|error| super::error::ShadowPageError::from(anyhow::Error::new(error)))?;
        // Projection is always replayed from durable state. An interruption
        // after capture but before projection recovers on the next observation.
        let source = self
            .db
            .active_catalog_source(self.owner.clone(), id, MAX_CHANGES_PAGE_BYTES)
            .await
            .map_err(|error| super::error::ShadowPageError::from(anyhow::Error::new(error)))?;
        if let Some(source) = source {
            self.project(source).await?;
        }
        Ok(Some(id))
    }
}
