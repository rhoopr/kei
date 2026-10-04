//! Provider capture composition; no selection or checkpoint policy lives here.

use std::sync::Arc;

use serde_json::{Value, json};

use crate::state::SqliteStateDb;
use crate::state::db::{account::AccountOwner, provider_inbox::ObservedPage};

/// Operational safety bounds. Observations are retained, never evicted to fit.
pub(crate) const MAX_CHANGES_PAGE_BYTES: usize = 16 * 1024 * 1024;
const MAX_SHADOW_INBOX_BYTES: u64 = 512 * 1024 * 1024;

#[derive(Clone)]
pub(crate) struct ShadowCapture {
    pub(crate) db: Arc<SqliteStateDb>,
    pub(crate) owner: AccountOwner,
    realm: Arc<str>,
    pub(crate) capacity: u64,
}

impl ShadowCapture {
    pub(crate) fn new(db: Arc<SqliteStateDb>, owner: AccountOwner, realm: &str) -> Self {
        Self {
            db,
            owner,
            realm: Arc::from(realm),
            capacity: MAX_SHADOW_INBOX_BYTES,
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

    pub(crate) async fn capture(&self, page: ObservedPage) -> anyhow::Result<()> {
        self.db
            .capture_shadow_page(self.owner.clone(), page, self.capacity)
            .await?;
        Ok(())
    }
}
