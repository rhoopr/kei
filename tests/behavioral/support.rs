//! Existing fixtures shared across behavioral areas; no command assertions or policy.

use crate::common;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

const TIMEOUT: Duration = Duration::from_secs(10);
static CLEAN_CMD_ID: AtomicUsize = AtomicUsize::new(0);

/// Helper: run kei with env scrubbed and a temp data-dir so it never
/// touches real config/cookies.
pub(super) fn clean_cmd() -> assert_cmd::Command {
    let mut cmd = common::cmd();
    let default_data_dir = std::env::temp_dir().join(format!(
        "kei-behavioral-{}-{}",
        std::process::id(),
        CLEAN_CMD_ID.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&default_data_dir).unwrap();
    cmd.env_remove("ICLOUD_USERNAME")
        .env_remove("ICLOUD_PASSWORD")
        .env_remove("KEI_CONFIG")
        .env_remove("KEI_DATA_DIR")
        .env_remove("KEI_DOWNLOAD_DIR")
        .env_remove("KEI_LOG_LEVEL")
        .env_remove("KEI_NO_AUTO_CONFIG")
        .env_remove("KEI_UNSTABLE_FAKE_TWO_FACTOR_REQUIRED_FOR_TESTS")
        .env("KEI_DATA_DIR", default_data_dir)
        .timeout(TIMEOUT);
    cmd
}

pub(super) fn write_sync_config(config_path: &std::path::Path, download_dir: &str) {
    std::fs::write(
        config_path,
        format!(
            "[download]\ndirectory = {}\n",
            common::toml_string(download_dir)
        ),
    )
    .unwrap();
}

/// Account namespace codec mirrored for synthetic CLI fixtures in the com realm.
pub(super) fn sanitize_username(username: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hash = Sha256::new();
    hash.update(b"configured-account-v1");
    for value in ["com", username] {
        hash.update(value.len().to_string().as_bytes());
        hash.update(b":");
        hash.update(value.as_bytes());
    }
    format!("account-v1-{:x}", hash.finalize())
}

pub(super) fn bind_synthetic_owner(conn: &rusqlite::Connection, username: &str) {
    conn.execute_batch("CREATE TABLE account_owner (singleton INTEGER PRIMARY KEY CHECK(singleton=1), version INTEGER NOT NULL, account_key TEXT NOT NULL, provider_key TEXT NOT NULL);").unwrap();
    use sha2::{Digest, Sha256};
    let mut hash = Sha256::new();
    hash.update(b"authenticated-provider-v1");
    for value in ["com", "synthetic-cli-provider"] {
        hash.update(value.len().to_string().as_bytes());
        hash.update(b":");
        hash.update(value.as_bytes());
    }
    conn.execute(
        "INSERT INTO account_owner VALUES (1,1,?1,?2)",
        (
            sanitize_username(username),
            format!("{:x}", hash.finalize()),
        ),
    )
    .unwrap();
}

/// Schema version mirrored by `create_state_db` below. Must equal
/// `crate::state::schema::SCHEMA_VERSION` (the production constant). The
/// `state::behavioral_helper_schema_matches_production` test pins this so
/// any schema bump in `src/state/schema.rs` fails the suite until this
/// helper is updated to match, preventing silent drift between the
/// helper's "fresh DB" shape and what the binary expects.
pub(super) const HELPER_SCHEMA_VERSION: i32 = 35;

/// Create a state DB at the expected path for the given username inside
/// `data_dir`. Mirrors the current schema from `src/state/schema.rs`
/// so the binary's migrate() loop is a no-op when it opens these DBs,
/// i.e. tests run against the same shape
/// production code writes on a fresh install. Bump `HELPER_SCHEMA_VERSION`
/// and the DDL below together whenever schema.rs changes; the
/// `state::behavioral_helper_schema_matches_production` meta test enforces it.
pub(super) fn create_state_db(data_dir: &std::path::Path, username: &str) -> rusqlite::Connection {
    let db_name = format!("{}.db", sanitize_username(username));
    let db_path = data_dir.join(db_name);
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    bind_synthetic_owner(&conn, username);
    conn.execute_batch(
        r"

CREATE TABLE IF NOT EXISTS provider_active_generations (
 id TEXT PRIMARY KEY, account_key TEXT NOT NULL, provider_key TEXT NOT NULL,
 scope TEXT NOT NULL, config_hash TEXT NOT NULL, basis TEXT NOT NULL,
 metadata_enabled INTEGER NOT NULL CHECK(metadata_enabled IN (0,1)),
 specification BLOB NOT NULL, specification_hash TEXT NOT NULL,
 sealed INTEGER NOT NULL DEFAULT 0 CHECK(sealed IN (0,1)),
 checkpoint_ready INTEGER NOT NULL DEFAULT 0 CHECK(checkpoint_ready IN (0,1)),
 checkpoint_veto TEXT, seal_hash TEXT, seal_header_hash TEXT, created_at INTEGER NOT NULL,
 replay_after TEXT, last_replayed_at INTEGER NOT NULL DEFAULT 0 CHECK(last_replayed_at>=0),
 charged_bytes INTEGER NOT NULL CHECK(charged_bytes>0)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS provider_selection_rank_pages (
 id TEXT PRIMARY KEY, generation TEXT NOT NULL REFERENCES provider_active_generations(id),
 pass_key TEXT NOT NULL, request BLOB NOT NULL, request_hash TEXT NOT NULL,
 body BLOB NOT NULL, body_hash TEXT NOT NULL,
 charged_bytes INTEGER NOT NULL CHECK(charged_bytes>0),
 UNIQUE(generation,pass_key,request_hash,body_hash)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS provider_selection_rank_records (
 page_id TEXT NOT NULL REFERENCES provider_selection_rank_pages(id),
 ordinal INTEGER NOT NULL CHECK(ordinal>=0), record_name TEXT NOT NULL,
 record_type TEXT, deleted INTEGER NOT NULL CHECK(deleted IN (0,1)),
 PRIMARY KEY(page_id,ordinal)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS idx_selection_rank_identity ON provider_selection_rank_records(record_name,page_id);
CREATE TABLE IF NOT EXISTS provider_active_decisions (
 generation TEXT NOT NULL REFERENCES provider_active_generations(id), pass_key TEXT NOT NULL,
 child TEXT NOT NULL, manifest BLOB NOT NULL, manifest_hash TEXT NOT NULL,
 outcome TEXT NOT NULL CHECK(outcome IN ('selected','excluded','deferred')),
 admission TEXT NOT NULL CHECK(admission IN ('admitted','excluded','deferred')),
 reason TEXT NOT NULL, admission_hash TEXT, attempts INTEGER NOT NULL DEFAULT 0,
 next_retry_at INTEGER NOT NULL DEFAULT 0,
 charged_bytes INTEGER NOT NULL CHECK(charged_bytes>0),
 PRIMARY KEY(generation,pass_key,child)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS provider_active_sources (
 generation TEXT NOT NULL, pass_key TEXT NOT NULL, child TEXT NOT NULL,
 page_id TEXT NOT NULL, ordinal INTEGER NOT NULL,
 PRIMARY KEY(generation,pass_key,child,page_id,ordinal),
 FOREIGN KEY(generation,pass_key,child) REFERENCES provider_active_decisions(generation,pass_key,child),
 FOREIGN KEY(page_id,ordinal) REFERENCES provider_selection_rank_records(page_id,ordinal)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS provider_active_destinations (
 generation TEXT NOT NULL, pass_key TEXT NOT NULL, child TEXT NOT NULL,
 library TEXT NOT NULL, asset_id TEXT NOT NULL, master TEXT NOT NULL,
 version_size TEXT NOT NULL, path TEXT NOT NULL, compat_path TEXT NOT NULL,
 checksum TEXT NOT NULL, size_bytes INTEGER NOT NULL CHECK(size_bytes>=0),
 created_at REAL NOT NULL, added_at REAL, metadata_hash TEXT NOT NULL,
 admitted INTEGER NOT NULL CHECK(admitted IN (0,1)),
 verified_media INTEGER NOT NULL DEFAULT 0 CHECK(verified_media IN (0,1)),
 verified_metadata INTEGER NOT NULL DEFAULT 0 CHECK(verified_metadata IN (0,1)),
 local_checksum TEXT, source_checksum TEXT, grouping_hash TEXT, intent_hash TEXT, progress_hash TEXT,
 prepared_checksum TEXT, prepared_size INTEGER, prepared_hash TEXT,
 CHECK((prepared_checksum IS NULL AND prepared_size IS NULL AND prepared_hash IS NULL)
    OR (prepared_checksum IS NOT NULL AND prepared_size IS NOT NULL AND prepared_hash IS NOT NULL
        AND length(prepared_checksum)=64 AND prepared_size>=0 AND length(prepared_hash)=64)),
 PRIMARY KEY(generation,pass_key,child,version_size,path),
 FOREIGN KEY(generation,pass_key,child) REFERENCES provider_active_decisions(generation,pass_key,child)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS idx_active_destination_identity ON provider_active_destinations(library,asset_id,version_size,admitted,verified_media,verified_metadata);

CREATE TABLE IF NOT EXISTS provider_selection_generations (
    id TEXT PRIMARY KEY,
    account_key TEXT NOT NULL,
    provider_key TEXT NOT NULL,
    format INTEGER NOT NULL CHECK(format=1),
    scope TEXT NOT NULL,
    config_hash TEXT NOT NULL,
    manifest BLOB NOT NULL,
    charged_bytes INTEGER NOT NULL CHECK(charged_bytes>0)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS provider_selection_sources (
    generation TEXT NOT NULL REFERENCES provider_selection_generations(id),
    page_id INTEGER NOT NULL,
    ordinal INTEGER NOT NULL,
    body_hash TEXT NOT NULL,
    PRIMARY KEY(generation,page_id,ordinal),
    FOREIGN KEY(page_id,ordinal) REFERENCES provider_catalog_records(page_id,ordinal)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS provider_selection_decisions (
    generation TEXT NOT NULL REFERENCES provider_selection_generations(id),
    pass_key TEXT NOT NULL,
    child TEXT NOT NULL,
    outcome TEXT NOT NULL CHECK(outcome IN ('selected','excluded','deferred')),
    reason TEXT NOT NULL,
    PRIMARY KEY(generation,pass_key,child)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS provider_selection_destinations (
    generation TEXT NOT NULL,
    pass_key TEXT NOT NULL,
    child TEXT NOT NULL,
    version_size TEXT NOT NULL,
    path TEXT NOT NULL,
    checksum TEXT NOT NULL,
    size_bytes INTEGER NOT NULL CHECK(size_bytes>=0),
    metadata_hash TEXT NOT NULL,
    PRIMARY KEY(generation,pass_key,child,version_size,path),
    FOREIGN KEY(generation,pass_key,child) REFERENCES provider_selection_decisions(generation,pass_key,child)
) WITHOUT ROWID;

CREATE TABLE IF NOT EXISTS provider_shadow_pages (
    id INTEGER PRIMARY KEY,
    account_key TEXT NOT NULL,
    provider_key TEXT NOT NULL,
    scope TEXT NOT NULL,
    request_cursor TEXT NOT NULL,
    successor TEXT NOT NULL CHECK(length(trim(successor)) > 0),
    more_coming INTEGER NOT NULL CHECK(more_coming IN (0,1)),
    body_hash TEXT NOT NULL,
    body BLOB NOT NULL,
    charged_bytes INTEGER NOT NULL CHECK(charged_bytes > 0),
    observed_at INTEGER NOT NULL,
    UNIQUE(scope,request_cursor,body_hash)
);
CREATE TABLE IF NOT EXISTS provider_shadow_records (
    page_id INTEGER NOT NULL REFERENCES provider_shadow_pages(id),
    ordinal INTEGER NOT NULL CHECK(ordinal >= 0),
    record_name TEXT NOT NULL CHECK(length(trim(record_name)) > 0),
    record_type TEXT,
    deleted INTEGER NOT NULL CHECK(deleted IN (0,1)),
    PRIMARY KEY(page_id,ordinal)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS provider_shadow_receipts (
    scope TEXT PRIMARY KEY,
    page_id INTEGER NOT NULL REFERENCES provider_shadow_pages(id)
) WITHOUT ROWID;


CREATE TABLE IF NOT EXISTS provider_work_receipts (
    page_id INTEGER NOT NULL,
    ordinal INTEGER NOT NULL,
    config_hash TEXT NOT NULL,
    confirmation_hash TEXT NOT NULL,
    confirmation BLOB,
    state TEXT NOT NULL CHECK(state IN ('admitted','deferred')),
    reason TEXT NOT NULL,
    scope TEXT NOT NULL,
    source_hash TEXT NOT NULL,
    master_record_name TEXT,
    charged_bytes INTEGER NOT NULL CHECK(charged_bytes>0),
    PRIMARY KEY(page_id,ordinal,config_hash,confirmation_hash),
    FOREIGN KEY(page_id,ordinal) REFERENCES provider_catalog_records(page_id,ordinal)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS provider_work_obligations (
    page_id INTEGER NOT NULL,
    ordinal INTEGER NOT NULL,
    config_hash TEXT NOT NULL,
    confirmation_hash TEXT NOT NULL,
    version_size TEXT NOT NULL,
    library TEXT NOT NULL,
    asset_id TEXT NOT NULL,
    checksum TEXT NOT NULL,
    size_bytes INTEGER NOT NULL,
    filename TEXT NOT NULL,
    created_at REAL NOT NULL,
    added_at REAL,
    metadata_hash TEXT NOT NULL,
    PRIMARY KEY(page_id,ordinal,config_hash,confirmation_hash,version_size),
    FOREIGN KEY(page_id,ordinal,config_hash,confirmation_hash) REFERENCES provider_work_receipts(page_id,ordinal,config_hash,confirmation_hash) DEFERRABLE INITIALLY DEFERRED
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS idx_provider_work_queue ON provider_work_obligations(library,asset_id,version_size);
CREATE TABLE IF NOT EXISTS provider_work_retries (
    page_id INTEGER NOT NULL,
    ordinal INTEGER NOT NULL,
    config_hash TEXT NOT NULL,
    attempts INTEGER NOT NULL CHECK(attempts BETWEEN 1 AND 32),
    last_attempt_at INTEGER NOT NULL,
    next_retry_at INTEGER NOT NULL CHECK(next_retry_at > last_attempt_at),
    PRIMARY KEY(page_id,ordinal,config_hash),
    FOREIGN KEY(page_id,ordinal) REFERENCES provider_catalog_records(page_id,ordinal)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS provider_work_scan (
    scope TEXT NOT NULL,
    config_hash TEXT NOT NULL,
    page_id INTEGER NOT NULL,
    ordinal INTEGER NOT NULL,
    PRIMARY KEY(scope,config_hash)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS provider_catalog_records (
    page_id INTEGER NOT NULL,
    ordinal INTEGER NOT NULL,
    record_name TEXT NOT NULL,
    record_type TEXT,
    deleted INTEGER NOT NULL CHECK(deleted IN (0,1)),
    kind TEXT NOT NULL CHECK(kind IN ('master','asset','album','relation','unknown')),
    PRIMARY KEY(page_id,ordinal),
    FOREIGN KEY(page_id,ordinal) REFERENCES provider_shadow_records(page_id,ordinal)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS provider_catalog_references (
    page_id INTEGER NOT NULL,
    ordinal INTEGER NOT NULL,
    field_path TEXT NOT NULL,
    target_record_name TEXT NOT NULL CHECK(length(trim(target_record_name))>0),
    target_zone_name TEXT,
    target_zone_owner TEXT,
    PRIMARY KEY(page_id,ordinal,field_path),
    FOREIGN KEY(page_id,ordinal) REFERENCES provider_catalog_records(page_id,ordinal)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS provider_catalog_debt (
    page_id INTEGER NOT NULL,
    ordinal INTEGER NOT NULL,
    field_path TEXT NOT NULL,
    reason TEXT NOT NULL,
    PRIMARY KEY(page_id,ordinal,field_path,reason),
    FOREIGN KEY(page_id,ordinal) REFERENCES provider_catalog_records(page_id,ordinal)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS provider_catalog_pages (
    page_id INTEGER PRIMARY KEY REFERENCES provider_shadow_pages(id),
    projector_version INTEGER NOT NULL CHECK(projector_version>0),
    body_hash TEXT NOT NULL,
    record_count INTEGER NOT NULL CHECK(record_count>=0),
    reference_count INTEGER NOT NULL CHECK(reference_count>=0),
    debt_count INTEGER NOT NULL CHECK(debt_count>=0),
    charged_bytes INTEGER NOT NULL CHECK(charged_bytes>0)
);

CREATE TABLE IF NOT EXISTS asset_metadata_paths (
    library TEXT NOT NULL,
    id TEXT NOT NULL,
    version_size TEXT NOT NULL,
    local_path TEXT NOT NULL,
    provider_checksum TEXT NOT NULL,
    local_checksum TEXT,
    download_checksum TEXT,
    source_checksum TEXT,
    metadata_write_failed_at INTEGER,
    capture_repair_metadata_hash TEXT,
    capture_repair_output_checksum TEXT,
    capture_repair_output_size INTEGER,
    PRIMARY KEY (library, id, version_size, local_path)
);
CREATE INDEX IF NOT EXISTS idx_asset_metadata_paths_retry
    ON asset_metadata_paths(metadata_write_failed_at, library, id, version_size, local_path)
    WHERE metadata_write_failed_at IS NOT NULL;
        CREATE TABLE IF NOT EXISTS assets (
            library TEXT NOT NULL,
            id TEXT NOT NULL,
            version_size TEXT NOT NULL,
            checksum TEXT NOT NULL,
            filename TEXT NOT NULL,
            created_at INTEGER NOT NULL,
            added_at INTEGER,
            size_bytes INTEGER NOT NULL,
            media_type TEXT NOT NULL,
            status TEXT NOT NULL DEFAULT 'pending',
            downloaded_at INTEGER,
            local_path TEXT,
            last_seen_at INTEGER NOT NULL,
            download_attempts INTEGER DEFAULT 0,
            last_error TEXT,
            local_checksum TEXT,
            download_checksum TEXT,
            source TEXT NOT NULL DEFAULT 'icloud',
            is_favorite INTEGER NOT NULL DEFAULT 0,
            rating INTEGER,
            latitude REAL,
            longitude REAL,
            altitude REAL,
            orientation INTEGER,
            duration_secs REAL,
            timezone_offset INTEGER,
            width INTEGER,
            height INTEGER,
            title TEXT,
            keywords TEXT,
            description TEXT,
            media_subtype TEXT,
            burst_id TEXT,
            is_hidden INTEGER NOT NULL DEFAULT 0,
            is_archived INTEGER NOT NULL DEFAULT 0,
            modified_at INTEGER,
            is_deleted INTEGER NOT NULL DEFAULT 0,
            deleted_at INTEGER,
            provider_data TEXT,
            metadata_hash TEXT,
            metadata_write_failed_at INTEGER,
            imported_size INTEGER,
            imported_mtime INTEGER,
            capture_repair_metadata_hash TEXT,
            capture_repair_output_checksum TEXT,
            capture_repair_output_size INTEGER,
            PRIMARY KEY (library, id, version_size)
        );
        CREATE INDEX IF NOT EXISTS idx_assets_status ON assets(status);
        CREATE INDEX IF NOT EXISTS idx_assets_local_path ON assets(local_path);
        CREATE INDEX IF NOT EXISTS idx_assets_checksum ON assets(checksum);
        CREATE INDEX IF NOT EXISTS idx_assets_metadata_hash
            ON assets (metadata_hash) WHERE status = 'downloaded';

        CREATE TABLE IF NOT EXISTS sync_runs (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            started_at INTEGER NOT NULL,
            completed_at INTEGER,
            assets_seen INTEGER DEFAULT 0,
            assets_downloaded INTEGER DEFAULT 0,
            assets_failed INTEGER DEFAULT 0,
            interrupted INTEGER DEFAULT 0,
            status TEXT NOT NULL DEFAULT 'running',
            enumeration_errors INTEGER NOT NULL DEFAULT 0,
            api_total_at_start INTEGER,
            api_total_at_start_partial INTEGER NOT NULL DEFAULT 0,
            inventory_drop_detected INTEGER NOT NULL DEFAULT 0,
            inventory_drop_previous_total INTEGER,
            inventory_drop_current_total INTEGER,
            inventory_drop_library TEXT
        );

        CREATE TABLE IF NOT EXISTS metadata (
            key TEXT PRIMARY KEY NOT NULL,
            value TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS asset_albums (
            library    TEXT NOT NULL,
            asset_id   TEXT NOT NULL,
            album_name TEXT NOT NULL,
            source     TEXT NOT NULL,
            PRIMARY KEY (library, asset_id, album_name, source)
        );
        CREATE INDEX IF NOT EXISTS idx_asset_albums_lookup
            ON asset_albums (library, asset_id);

        CREATE TABLE IF NOT EXISTS asset_people (
            library     TEXT NOT NULL,
            asset_id    TEXT NOT NULL,
            person_name TEXT NOT NULL,
            PRIMARY KEY (library, asset_id, person_name)
        );
        CREATE INDEX IF NOT EXISTS idx_asset_people_lookup
            ON asset_people (library, asset_id);

        CREATE TABLE IF NOT EXISTS album_containers (
            library TEXT NOT NULL,
            container_id TEXT NOT NULL,
            album_name TEXT NOT NULL,
            pass_kind TEXT NOT NULL,
            is_deleted INTEGER NOT NULL DEFAULT 0,
            updated_at INTEGER NOT NULL,
            PRIMARY KEY (library, container_id)
        );

        CREATE TABLE IF NOT EXISTS album_membership_snapshots (
            library TEXT NOT NULL,
            container_id TEXT NOT NULL,
            generation INTEGER NOT NULL,
            status TEXT NOT NULL,
            enum_config_hash TEXT,
            started_at INTEGER NOT NULL,
            completed_at INTEGER,
            PRIMARY KEY (library, container_id, generation)
        );

        CREATE TABLE IF NOT EXISTS asset_album_memberships (
            library TEXT NOT NULL,
            asset_record_name TEXT NOT NULL,
            master_record_name TEXT,
            container_id TEXT NOT NULL,
            generation INTEGER NOT NULL,
            is_deleted INTEGER NOT NULL DEFAULT 0,
            source TEXT NOT NULL,
            updated_at INTEGER NOT NULL,
            PRIMARY KEY (library, asset_record_name, container_id)
        );

        CREATE INDEX IF NOT EXISTS idx_album_containers_lookup
            ON album_containers (library, album_name);
        CREATE INDEX IF NOT EXISTS idx_album_membership_snapshots_status
            ON album_membership_snapshots (library, container_id, status);
        CREATE INDEX IF NOT EXISTS idx_asset_album_memberships_asset
            ON asset_album_memberships (library, asset_record_name, is_deleted);
        CREATE INDEX IF NOT EXISTS idx_asset_album_memberships_container
            ON asset_album_memberships (library, container_id, is_deleted);

        CREATE TABLE IF NOT EXISTS asset_master_mappings (
            library TEXT NOT NULL,
            asset_record_name TEXT NOT NULL,
            master_record_name TEXT NOT NULL,
            updated_at INTEGER NOT NULL,
            PRIMARY KEY (library, asset_record_name)
        );
        CREATE INDEX IF NOT EXISTS idx_asset_master_mappings_master
            ON asset_master_mappings (library, master_record_name);

        CREATE TABLE IF NOT EXISTS asset_verifications (
            library TEXT NOT NULL,
            id TEXT NOT NULL,
            version_size TEXT NOT NULL,
            state TEXT NOT NULL CHECK (state IN ('unknown', 'transient_failure')),
            reason TEXT NOT NULL,
            checked_at INTEGER NOT NULL,
            PRIMARY KEY (library, id, version_size),
            FOREIGN KEY (library, id, version_size)
                REFERENCES assets (library, id, version_size) ON DELETE CASCADE
        );
        CREATE INDEX IF NOT EXISTS idx_asset_verifications_state
            ON asset_verifications (state, checked_at);

        CREATE TABLE IF NOT EXISTS legacy_master_state_owners (
            library TEXT NOT NULL,
            master_record_name TEXT NOT NULL,
            asset_record_name TEXT NOT NULL,
            claimed_at INTEGER NOT NULL,
            PRIMARY KEY (library, master_record_name)
        );

        CREATE INDEX IF NOT EXISTS idx_legacy_master_state_owners_asset
            ON legacy_master_state_owners (library, asset_record_name, master_record_name);

        CREATE TABLE IF NOT EXISTS asset_metadata_capture_revisions (
            library TEXT NOT NULL,
            asset_id TEXT NOT NULL,
            revision INTEGER NOT NULL,
            updated_at INTEGER NOT NULL,
            PRIMARY KEY (library, asset_id)
        );

        CREATE TABLE IF NOT EXISTS metadata_capture_state (
            library TEXT PRIMARY KEY NOT NULL,
            active_revision INTEGER NOT NULL DEFAULT 0,
            pending_revision INTEGER,
            processed_assets INTEGER NOT NULL DEFAULT 0,
            failed_assets INTEGER NOT NULL DEFAULT 0,
            last_error TEXT,
            updated_at INTEGER NOT NULL
        );

        CREATE INDEX IF NOT EXISTS idx_asset_metadata_capture_revision
            ON asset_metadata_capture_revisions (library, revision);

        CREATE TABLE IF NOT EXISTS owned_temp_files (
            path BLOB PRIMARY KEY NOT NULL,
            claimed_at INTEGER NOT NULL
        );

        CREATE TABLE IF NOT EXISTS reconciliation_paths (
            library TEXT NOT NULL,
            id TEXT NOT NULL,
            version_size TEXT NOT NULL,
            requested_path_key TEXT NOT NULL,
            destination_path_key TEXT NOT NULL,
            destination_path TEXT NOT NULL,
            provider_checksum TEXT NOT NULL,
            provider_size INTEGER NOT NULL CHECK (provider_size >= 0 OR (provider_size = -1 AND provider_checksum = '')),
            PRIMARY KEY (library, id, version_size, requested_path_key, provider_checksum, provider_size)
        );
        CREATE INDEX IF NOT EXISTS idx_reconciliation_paths_destination
            ON reconciliation_paths(destination_path_key);

        CREATE TABLE IF NOT EXISTS scoped_db_sync_tokens (
            provider TEXT NOT NULL,
            account TEXT NOT NULL,
            shape_version INTEGER NOT NULL,
            scope_hash TEXT NOT NULL,
            selected_zones_json TEXT NOT NULL,
            scope_json TEXT NOT NULL,
            token TEXT NOT NULL,
            created_at INTEGER NOT NULL,
            updated_at INTEGER NOT NULL,
            PRIMARY KEY (provider, account, shape_version, scope_hash)
        );
        ",
    )
    .unwrap();
    conn.execute_batch(r"
CREATE TABLE IF NOT EXISTS unresolved_sparse_identities (
    library TEXT NOT NULL,
    source_record_name TEXT NOT NULL,
    original_evidence TEXT NOT NULL,
    observed_evidence TEXT NOT NULL,
    lookup_evidence TEXT,
    generation INTEGER NOT NULL CHECK(generation > 0),
    first_seen_at INTEGER NOT NULL,
    last_attempt_at INTEGER,
    attempts INTEGER NOT NULL DEFAULT 0 CHECK(attempts >= 0),
    next_retry_at INTEGER,
    last_outcome TEXT,
    PRIMARY KEY (library, source_record_name)
);
CREATE INDEX IF NOT EXISTS idx_sparse_identity_retry ON unresolved_sparse_identities(library, next_retry_at);

CREATE TABLE IF NOT EXISTS metadata_capture_retries (
    library TEXT NOT NULL,
    asset_id TEXT NOT NULL,
    target_revision INTEGER NOT NULL,
    evidence TEXT NOT NULL,
    generation INTEGER NOT NULL CHECK(generation > 0),
    attempts INTEGER NOT NULL CHECK(attempts > 0),
    last_attempt_at INTEGER NOT NULL,
    next_retry_at INTEGER NOT NULL,
    PRIMARY KEY (library, asset_id, target_revision)
);

").unwrap();
    conn.execute_batch(r"
CREATE TABLE IF NOT EXISTS unattributed_legacy (
    library TEXT NOT NULL,
    asset_id TEXT NOT NULL,
    evidence_version INTEGER NOT NULL CHECK(evidence_version = 1),
    original_evidence TEXT NOT NULL,
    files TEXT NOT NULL,
    prepared_at INTEGER NOT NULL,
    active_generation INTEGER,
    PRIMARY KEY(library, asset_id)
);
CREATE TABLE IF NOT EXISTS unattributed_legacy_paths (
    library TEXT NOT NULL,
    asset_id TEXT NOT NULL,
    path_key TEXT NOT NULL,
    local_path TEXT NOT NULL,
    PRIMARY KEY(library, asset_id, path_key)
);
CREATE UNIQUE INDEX IF NOT EXISTS unattributed_legacy_path_owner ON unattributed_legacy_paths(path_key);
CREATE TABLE IF NOT EXISTS unattributed_legacy_proofs (
    library TEXT NOT NULL,
    asset_id TEXT NOT NULL,
    generation INTEGER NOT NULL CHECK(generation > 0),
    evidence_version INTEGER NOT NULL CHECK(evidence_version = 1),
    dependency_evidence TEXT NOT NULL,
    provider_evidence TEXT NOT NULL,
    config_hash TEXT NOT NULL,
    prior_cursor TEXT NOT NULL,
    next_cursor TEXT NOT NULL,
    committed_at INTEGER NOT NULL,
    PRIMARY KEY(library, asset_id, generation)
);
CREATE TRIGGER IF NOT EXISTS unattributed_original_immutable BEFORE UPDATE ON unattributed_legacy
WHEN NEW.library IS NOT OLD.library OR NEW.asset_id IS NOT OLD.asset_id
 OR NEW.evidence_version IS NOT OLD.evidence_version OR NEW.original_evidence IS NOT OLD.original_evidence
 OR NEW.files IS NOT OLD.files OR NEW.prepared_at IS NOT OLD.prepared_at
BEGIN SELECT RAISE(ABORT, 'unattributed legacy evidence is immutable'); END;
CREATE TRIGGER IF NOT EXISTS unattributed_original_retained BEFORE DELETE ON unattributed_legacy
BEGIN SELECT RAISE(ABORT, 'unattributed legacy evidence must be retained'); END;
CREATE TRIGGER IF NOT EXISTS unattributed_proof_immutable BEFORE UPDATE ON unattributed_legacy_proofs
BEGIN SELECT RAISE(ABORT, 'unattributed legacy proof is immutable'); END;
CREATE TRIGGER IF NOT EXISTS unattributed_proof_retained BEFORE DELETE ON unattributed_legacy_proofs
BEGIN SELECT RAISE(ABORT, 'unattributed legacy proof must be retained'); END;
CREATE TRIGGER IF NOT EXISTS unattributed_path_immutable BEFORE UPDATE ON unattributed_legacy_paths
BEGIN SELECT RAISE(ABORT, 'unattributed legacy path is protected'); END;
CREATE TRIGGER IF NOT EXISTS unattributed_path_retained BEFORE DELETE ON unattributed_legacy_paths
BEGIN SELECT RAISE(ABORT, 'unattributed legacy path must be retained'); END;
").unwrap();
    // Defense in depth. Callers must exclude protected identities before byte
    // writes; refusing a late state write alone cannot protect file contents.
    for (table, id) in [
        ("assets", "id"),
        ("asset_metadata_paths", "id"),
        ("asset_metadata_capture_revisions", "asset_id"),
        ("metadata_capture_retries", "asset_id"),
        ("asset_albums", "asset_id"),
        ("asset_people", "asset_id"),
        ("asset_album_memberships", "asset_record_name"),
        ("legacy_master_state_owners", "master_record_name"),
    ] {
        for (action, row) in [("INSERT", "NEW"), ("UPDATE", "OLD"), ("DELETE", "OLD")] {
            let moved_into = if action == "UPDATE" {
                format!(
                    " OR EXISTS(SELECT 1 FROM unattributed_legacy p WHERE p.library=NEW.library AND p.asset_id=NEW.{id})"
                )
            } else {
                String::new()
            };
            conn.execute_batch(&format!(
                "CREATE TRIGGER IF NOT EXISTS unattributed_protect_{table}_{action} BEFORE {action} ON {table} \
                 WHEN EXISTS(SELECT 1 FROM unattributed_legacy p WHERE p.library={row}.library AND p.asset_id={row}.{id}){moved_into} \
                 BEGIN SELECT RAISE(ABORT, 'unattributed legacy identity is protected'); END;"
            )).unwrap();
        }
    }

    conn.execute_batch(r"
CREATE TABLE IF NOT EXISTS primary_layout_bindings (
 family TEXT PRIMARY KEY, library TEXT NOT NULL, source_library TEXT NOT NULL, child TEXT NOT NULL,
 generation INTEGER NOT NULL CHECK(generation > 0), decision TEXT NOT NULL,
 policy TEXT NOT NULL, binding BLOB NOT NULL
);
CREATE TABLE IF NOT EXISTS primary_layout_operations (
 operation TEXT PRIMARY KEY, family TEXT NOT NULL, library TEXT NOT NULL, source_library TEXT NOT NULL,
 prior_generation INTEGER NOT NULL, decision TEXT NOT NULL, policy TEXT NOT NULL,
 phase TEXT NOT NULL CHECK(phase IN ('planned','prepared','preserved','publishing','committed','conflict','cancelled')),
 header BLOB NOT NULL, conflict TEXT
);
CREATE UNIQUE INDEX IF NOT EXISTS primary_layout_one_pending ON primary_layout_operations(family) WHERE phase NOT IN ('committed','cancelled');
CREATE TABLE IF NOT EXISTS primary_layout_members (
 operation TEXT NOT NULL REFERENCES primary_layout_operations(operation),
 member INTEGER NOT NULL, evidence BLOB NOT NULL, PRIMARY KEY(operation,member)
);
CREATE TABLE IF NOT EXISTS primary_layout_claims (
 path_key TEXT PRIMARY KEY, family TEXT NOT NULL, operation TEXT,
 library TEXT NOT NULL, child TEXT NOT NULL, version TEXT NOT NULL,
 provider_checksum TEXT NOT NULL, local_checksum TEXT,
 native_path BLOB NOT NULL
);
CREATE TABLE IF NOT EXISTS primary_layout_preserved (
 operation TEXT NOT NULL, member INTEGER NOT NULL, native_path BLOB NOT NULL,
 media_checksum TEXT NOT NULL, sidecar_checksum TEXT, evidence BLOB NOT NULL,
 PRIMARY KEY(operation,member)
);
CREATE TABLE IF NOT EXISTS primary_layout_superseded_paths (
 library TEXT NOT NULL, child TEXT NOT NULL, version TEXT NOT NULL,
 provider_checksum TEXT NOT NULL, native_path BLOB NOT NULL, compat_path TEXT,
 PRIMARY KEY(library,child,version,provider_checksum,native_path)
);
CREATE INDEX IF NOT EXISTS primary_layout_superseded_compat ON primary_layout_superseded_paths(library,child,version,provider_checksum,compat_path);
CREATE INDEX IF NOT EXISTS primary_layout_binding_source ON primary_layout_bindings(source_library);
CREATE INDEX IF NOT EXISTS primary_layout_binding_owner ON primary_layout_bindings(library,child);
CREATE INDEX IF NOT EXISTS primary_layout_claim_owner ON primary_layout_claims(library,child);
CREATE INDEX IF NOT EXISTS primary_layout_pending_library ON primary_layout_operations(source_library,phase);
").unwrap();
    conn.pragma_update(None, "user_version", HELPER_SCHEMA_VERSION)
        .unwrap();
    conn
}

/// Insert an asset row into the state DB. The `library` column defaults
/// to `'PrimarySync'` to match the production v7→v8 backfill, where
/// pre-v8 rows (which had no library column) all came from PrimarySync.
pub(super) fn insert_asset(
    conn: &rusqlite::Connection,
    id: &str,
    status: &str,
    filename: &str,
    local_path: Option<&str>,
    last_error: Option<&str>,
    local_checksum: Option<&str>,
) {
    conn.execute(
        "INSERT INTO assets (library, id, version_size, checksum, filename, created_at, \
         size_bytes, media_type, status, local_path, last_seen_at, last_error, \
         local_checksum, downloaded_at) \
         VALUES ('PrimarySync', ?1, 'original', 'abc', ?2, 1700000000, 1000, 'photo', ?3, ?4, \
         1700000000, ?5, ?6, CASE WHEN ?3 = 'downloaded' THEN 1700000000 ELSE NULL END)",
        rusqlite::params![id, filename, status, local_path, last_error, local_checksum],
    )
    .unwrap();
}

pub(super) fn write_fake_two_factor_config(
    dir: &std::path::Path,
    username: &str,
) -> std::path::PathBuf {
    let download_dir = dir.join("photos");
    std::fs::create_dir_all(&download_dir).unwrap();
    // An explicit empty file bypasses the global OS credential store.
    // Release ignores the fake hook and must fail before Apple authentication.
    let password_file = dir.join("empty-password");
    std::fs::write(&password_file, "").unwrap();
    let config_path = dir.join("config.toml");
    std::fs::write(
        &config_path,
        format!(
            "[auth]\nusername = {}\npassword_file = {}\n\n[download]\ndirectory = {}\n",
            common::toml_string(username),
            common::toml_string(&password_file.to_string_lossy()),
            common::toml_string(&download_dir.to_string_lossy()),
        ),
    )
    .unwrap();
    config_path
}

/// Build a `kei sync` invocation pre-populated with username, fresh tempdir
/// config/data directories, and `--only-print-filenames` so the
/// run exits before auth. Returns the live `Command` so callers can append
/// flag-specific args. Tempdirs are leaked into the binary (which never
/// touches them, as these tests bail in `Config::build`).
pub(super) fn sync_cmd_for_validation() -> assert_cmd::Command {
    sync_cmd_for_config_body("")
}

pub(super) fn sync_cmd_for_config_body(body: &str) -> assert_cmd::Command {
    let dir = tempfile::tempdir().unwrap();
    let dl_dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(
        &config_path,
        format!(
            "[auth]\nusername = \"x@x.com\"\n\n[download]\ndirectory = {}\n{body}",
            common::toml_string(dl_dir.path().to_str().unwrap())
        ),
    )
    .unwrap();
    let mut cmd = clean_cmd();
    cmd.args(["sync", "--config", config_path.to_str().unwrap()]);
    // Tempdirs leak intentionally: tests bail before sync touches them, and
    // OS-level tmpfs cleanup handles the directories at process exit.
    let _ = dir.keep();
    let _ = dl_dir.keep();
    cmd
}
