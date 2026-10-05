//! Provider capture composition; no selection or checkpoint policy lives here.

use std::sync::Arc;

use serde_json::{Value, json};

use crate::state::SqliteStateDb;
use crate::state::db::{account::AccountOwner, provider_inbox::ObservedPage};

/// Operational safety bounds. Observations are retained, never evicted to fit.
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
}

impl ShadowCapture {
    pub(crate) fn new(db: Arc<SqliteStateDb>, owner: AccountOwner, realm: &str) -> Self {
        Self {
            db,
            owner,
            realm: Arc::from(realm),
            capacity: MAX_SHADOW_INBOX_BYTES,
            catalog_capacity: MAX_CATALOG_INDEX_BYTES,
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
        let plan = tokio::task::spawn_blocking(move || super::projection::plan(&capture, source))
            .await?
            .map_err(super::error::ShadowPageError::from)?;
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
        Ok(())
    }

    pub(crate) async fn capture(&self, page: ObservedPage) -> anyhow::Result<()> {
        let id = self
            .db
            .capture_shadow_page(self.owner.clone(), page, self.capacity)
            .await
            .map_err(|error| super::error::ShadowPageError::from(anyhow::Error::new(error)))?;
        let source = self
            .db
            .catalog_source(self.owner.clone(), id, MAX_CHANGES_PAGE_BYTES)
            .await
            .map_err(|error| super::error::ShadowPageError::from(anyhow::Error::new(error)))?;
        self.project(source).await
    }
}
