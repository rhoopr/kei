-- Schema 49 as created by kei v0.30.0-beta.3 (src/state/schema.rs migrate()), for upgrade tests.
-- Generated from that tag; account_owner is created by the test helper.
PRAGMA user_version = 49;
CREATE TABLE sync_runs (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    started_at INTEGER NOT NULL,
    completed_at INTEGER,
    assets_seen INTEGER DEFAULT 0,
    assets_downloaded INTEGER DEFAULT 0,
    assets_failed INTEGER DEFAULT 0,
    interrupted INTEGER DEFAULT 0
, status TEXT NOT NULL DEFAULT 'running', enumeration_errors INTEGER NOT NULL DEFAULT 0, api_total_at_start INTEGER, api_total_at_start_partial INTEGER NOT NULL DEFAULT 0, inventory_drop_detected INTEGER NOT NULL DEFAULT 0, inventory_drop_previous_total INTEGER, inventory_drop_current_total INTEGER, inventory_drop_library TEXT);
CREATE TABLE metadata (
    key TEXT PRIMARY KEY NOT NULL,
    value TEXT NOT NULL
);
CREATE TABLE "assets" (
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
    metadata_write_failed_at INTEGER, imported_size INTEGER, imported_mtime INTEGER, capture_repair_metadata_hash TEXT, capture_repair_output_checksum TEXT, capture_repair_output_size INTEGER,
    PRIMARY KEY (library, id, version_size)
);
CREATE INDEX idx_assets_status ON assets(status);
CREATE INDEX idx_assets_local_path ON assets(local_path);
CREATE INDEX idx_assets_checksum ON assets(checksum);
CREATE INDEX idx_assets_metadata_hash ON assets (metadata_hash) WHERE status = 'downloaded';
CREATE TABLE "asset_albums" (
    library    TEXT NOT NULL,
    asset_id   TEXT NOT NULL,
    album_name TEXT NOT NULL,
    source     TEXT NOT NULL,
    PRIMARY KEY (library, asset_id, album_name, source)
);
CREATE INDEX idx_asset_albums_lookup
    ON asset_albums (library, asset_id);
CREATE TABLE "asset_people" (
    library     TEXT NOT NULL,
    asset_id    TEXT NOT NULL,
    person_name TEXT NOT NULL,
    PRIMARY KEY (library, asset_id, person_name)
);
CREATE INDEX idx_asset_people_lookup
    ON asset_people (library, asset_id);
CREATE TABLE album_containers (
    library TEXT NOT NULL,
    container_id TEXT NOT NULL,
    album_name TEXT NOT NULL,
    pass_kind TEXT NOT NULL,
    is_deleted INTEGER NOT NULL DEFAULT 0,
    updated_at INTEGER NOT NULL, directory_binding BLOB,
    PRIMARY KEY (library, container_id)
);
CREATE TABLE album_membership_snapshots (
    library TEXT NOT NULL,
    container_id TEXT NOT NULL,
    generation INTEGER NOT NULL,
    status TEXT NOT NULL,
    enum_config_hash TEXT,
    started_at INTEGER NOT NULL,
    completed_at INTEGER,
    PRIMARY KEY (library, container_id, generation)
);
CREATE TABLE asset_album_memberships (
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
CREATE INDEX idx_album_containers_lookup
    ON album_containers (library, album_name);
CREATE INDEX idx_album_membership_snapshots_status
    ON album_membership_snapshots (library, container_id, status);
CREATE INDEX idx_asset_album_memberships_asset
    ON asset_album_memberships (library, asset_record_name, is_deleted);
CREATE INDEX idx_asset_album_memberships_container
    ON asset_album_memberships (library, container_id, is_deleted);
CREATE TABLE scoped_db_sync_tokens (
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
CREATE TABLE asset_master_mappings (
    library TEXT NOT NULL,
    asset_record_name TEXT NOT NULL,
    master_record_name TEXT NOT NULL,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (library, asset_record_name)
);
CREATE INDEX idx_asset_master_mappings_master
    ON asset_master_mappings (library, master_record_name);
CREATE TABLE asset_verifications (
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
CREATE INDEX idx_asset_verifications_state
    ON asset_verifications (state, checked_at);
CREATE TABLE legacy_master_state_owners (
    library TEXT NOT NULL,
    master_record_name TEXT NOT NULL,
    asset_record_name TEXT NOT NULL,
    claimed_at INTEGER NOT NULL,
    PRIMARY KEY (library, master_record_name)
);
CREATE TABLE asset_metadata_capture_revisions (
    library TEXT NOT NULL,
    asset_id TEXT NOT NULL,
    revision INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (library, asset_id)
);
CREATE TABLE metadata_capture_state (
    library TEXT PRIMARY KEY NOT NULL,
    active_revision INTEGER NOT NULL DEFAULT 0,
    pending_revision INTEGER,
    processed_assets INTEGER NOT NULL DEFAULT 0,
    failed_assets INTEGER NOT NULL DEFAULT 0,
    last_error TEXT,
    updated_at INTEGER NOT NULL
);
CREATE INDEX idx_asset_metadata_capture_revision
    ON asset_metadata_capture_revisions (library, revision);
CREATE TABLE owned_temp_files (
    path BLOB PRIMARY KEY NOT NULL,
    claimed_at INTEGER NOT NULL
);
CREATE TABLE asset_metadata_paths (
    library TEXT NOT NULL,
    id TEXT NOT NULL,
    version_size TEXT NOT NULL,
    local_path TEXT NOT NULL,
    provider_checksum TEXT NOT NULL,
    local_checksum TEXT,
    download_checksum TEXT,
    metadata_write_failed_at INTEGER,
    capture_repair_metadata_hash TEXT,
    capture_repair_output_checksum TEXT,
    capture_repair_output_size INTEGER, source_checksum TEXT, embedded_metadata_receipt TEXT,
    PRIMARY KEY (library, id, version_size, local_path)
);
CREATE INDEX idx_asset_metadata_paths_retry
    ON asset_metadata_paths(metadata_write_failed_at, library, id, version_size, local_path)
    WHERE metadata_write_failed_at IS NOT NULL;
CREATE INDEX idx_legacy_master_state_owners_asset ON legacy_master_state_owners (library, asset_record_name, master_record_name);
CREATE TABLE "reconciliation_paths" (
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
CREATE INDEX idx_reconciliation_paths_destination ON reconciliation_paths(destination_path_key);
CREATE TABLE unresolved_sparse_identities (
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
CREATE INDEX idx_sparse_identity_retry ON unresolved_sparse_identities(library, next_retry_at);
CREATE TABLE metadata_capture_retries (
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
CREATE TABLE unattributed_legacy (
    library TEXT NOT NULL,
    asset_id TEXT NOT NULL,
    evidence_version INTEGER NOT NULL CHECK(evidence_version = 1),
    original_evidence TEXT NOT NULL,
    files TEXT NOT NULL,
    prepared_at INTEGER NOT NULL,
    active_generation INTEGER,
    PRIMARY KEY(library, asset_id)
);
CREATE TABLE unattributed_legacy_paths (
    library TEXT NOT NULL,
    asset_id TEXT NOT NULL,
    path_key TEXT NOT NULL,
    local_path TEXT NOT NULL,
    PRIMARY KEY(library, asset_id, path_key)
);
CREATE UNIQUE INDEX unattributed_legacy_path_owner ON unattributed_legacy_paths(path_key);
CREATE TABLE unattributed_legacy_proofs (
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
CREATE TRIGGER unattributed_original_immutable BEFORE UPDATE ON unattributed_legacy
WHEN NEW.library IS NOT OLD.library OR NEW.asset_id IS NOT OLD.asset_id
 OR NEW.evidence_version IS NOT OLD.evidence_version OR NEW.original_evidence IS NOT OLD.original_evidence
 OR NEW.files IS NOT OLD.files OR NEW.prepared_at IS NOT OLD.prepared_at
BEGIN SELECT RAISE(ABORT, 'unattributed legacy evidence is immutable'); END;
CREATE TRIGGER unattributed_original_retained BEFORE DELETE ON unattributed_legacy
BEGIN SELECT RAISE(ABORT, 'unattributed legacy evidence must be retained'); END;
CREATE TRIGGER unattributed_proof_immutable BEFORE UPDATE ON unattributed_legacy_proofs
BEGIN SELECT RAISE(ABORT, 'unattributed legacy proof is immutable'); END;
CREATE TRIGGER unattributed_proof_retained BEFORE DELETE ON unattributed_legacy_proofs
BEGIN SELECT RAISE(ABORT, 'unattributed legacy proof must be retained'); END;
CREATE TRIGGER unattributed_path_immutable BEFORE UPDATE ON unattributed_legacy_paths
BEGIN SELECT RAISE(ABORT, 'unattributed legacy path is protected'); END;
CREATE TRIGGER unattributed_path_retained BEFORE DELETE ON unattributed_legacy_paths
BEGIN SELECT RAISE(ABORT, 'unattributed legacy path must be retained'); END;
CREATE TRIGGER unattributed_protect_assets_INSERT BEFORE INSERT ON assets WHEN EXISTS(SELECT 1 FROM unattributed_legacy p WHERE p.library=NEW.library AND p.asset_id=NEW.id) BEGIN SELECT RAISE(ABORT, 'unattributed legacy identity is protected'); END;
CREATE TRIGGER unattributed_protect_assets_UPDATE BEFORE UPDATE ON assets WHEN EXISTS(SELECT 1 FROM unattributed_legacy p WHERE p.library=OLD.library AND p.asset_id=OLD.id) OR EXISTS(SELECT 1 FROM unattributed_legacy p WHERE p.library=NEW.library AND p.asset_id=NEW.id) BEGIN SELECT RAISE(ABORT, 'unattributed legacy identity is protected'); END;
CREATE TRIGGER unattributed_protect_assets_DELETE BEFORE DELETE ON assets WHEN EXISTS(SELECT 1 FROM unattributed_legacy p WHERE p.library=OLD.library AND p.asset_id=OLD.id) BEGIN SELECT RAISE(ABORT, 'unattributed legacy identity is protected'); END;
CREATE TRIGGER unattributed_protect_asset_metadata_paths_INSERT BEFORE INSERT ON asset_metadata_paths WHEN EXISTS(SELECT 1 FROM unattributed_legacy p WHERE p.library=NEW.library AND p.asset_id=NEW.id) BEGIN SELECT RAISE(ABORT, 'unattributed legacy identity is protected'); END;
CREATE TRIGGER unattributed_protect_asset_metadata_paths_UPDATE BEFORE UPDATE ON asset_metadata_paths WHEN EXISTS(SELECT 1 FROM unattributed_legacy p WHERE p.library=OLD.library AND p.asset_id=OLD.id) OR EXISTS(SELECT 1 FROM unattributed_legacy p WHERE p.library=NEW.library AND p.asset_id=NEW.id) BEGIN SELECT RAISE(ABORT, 'unattributed legacy identity is protected'); END;
CREATE TRIGGER unattributed_protect_asset_metadata_paths_DELETE BEFORE DELETE ON asset_metadata_paths WHEN EXISTS(SELECT 1 FROM unattributed_legacy p WHERE p.library=OLD.library AND p.asset_id=OLD.id) BEGIN SELECT RAISE(ABORT, 'unattributed legacy identity is protected'); END;
CREATE TRIGGER unattributed_protect_asset_metadata_capture_revisions_INSERT BEFORE INSERT ON asset_metadata_capture_revisions WHEN EXISTS(SELECT 1 FROM unattributed_legacy p WHERE p.library=NEW.library AND p.asset_id=NEW.asset_id) BEGIN SELECT RAISE(ABORT, 'unattributed legacy identity is protected'); END;
CREATE TRIGGER unattributed_protect_asset_metadata_capture_revisions_UPDATE BEFORE UPDATE ON asset_metadata_capture_revisions WHEN EXISTS(SELECT 1 FROM unattributed_legacy p WHERE p.library=OLD.library AND p.asset_id=OLD.asset_id) OR EXISTS(SELECT 1 FROM unattributed_legacy p WHERE p.library=NEW.library AND p.asset_id=NEW.asset_id) BEGIN SELECT RAISE(ABORT, 'unattributed legacy identity is protected'); END;
CREATE TRIGGER unattributed_protect_asset_metadata_capture_revisions_DELETE BEFORE DELETE ON asset_metadata_capture_revisions WHEN EXISTS(SELECT 1 FROM unattributed_legacy p WHERE p.library=OLD.library AND p.asset_id=OLD.asset_id) BEGIN SELECT RAISE(ABORT, 'unattributed legacy identity is protected'); END;
CREATE TRIGGER unattributed_protect_metadata_capture_retries_INSERT BEFORE INSERT ON metadata_capture_retries WHEN EXISTS(SELECT 1 FROM unattributed_legacy p WHERE p.library=NEW.library AND p.asset_id=NEW.asset_id) BEGIN SELECT RAISE(ABORT, 'unattributed legacy identity is protected'); END;
CREATE TRIGGER unattributed_protect_metadata_capture_retries_UPDATE BEFORE UPDATE ON metadata_capture_retries WHEN EXISTS(SELECT 1 FROM unattributed_legacy p WHERE p.library=OLD.library AND p.asset_id=OLD.asset_id) OR EXISTS(SELECT 1 FROM unattributed_legacy p WHERE p.library=NEW.library AND p.asset_id=NEW.asset_id) BEGIN SELECT RAISE(ABORT, 'unattributed legacy identity is protected'); END;
CREATE TRIGGER unattributed_protect_metadata_capture_retries_DELETE BEFORE DELETE ON metadata_capture_retries WHEN EXISTS(SELECT 1 FROM unattributed_legacy p WHERE p.library=OLD.library AND p.asset_id=OLD.asset_id) BEGIN SELECT RAISE(ABORT, 'unattributed legacy identity is protected'); END;
CREATE TRIGGER unattributed_protect_asset_albums_INSERT BEFORE INSERT ON asset_albums WHEN EXISTS(SELECT 1 FROM unattributed_legacy p WHERE p.library=NEW.library AND p.asset_id=NEW.asset_id) BEGIN SELECT RAISE(ABORT, 'unattributed legacy identity is protected'); END;
CREATE TRIGGER unattributed_protect_asset_albums_UPDATE BEFORE UPDATE ON asset_albums WHEN EXISTS(SELECT 1 FROM unattributed_legacy p WHERE p.library=OLD.library AND p.asset_id=OLD.asset_id) OR EXISTS(SELECT 1 FROM unattributed_legacy p WHERE p.library=NEW.library AND p.asset_id=NEW.asset_id) BEGIN SELECT RAISE(ABORT, 'unattributed legacy identity is protected'); END;
CREATE TRIGGER unattributed_protect_asset_albums_DELETE BEFORE DELETE ON asset_albums WHEN EXISTS(SELECT 1 FROM unattributed_legacy p WHERE p.library=OLD.library AND p.asset_id=OLD.asset_id) BEGIN SELECT RAISE(ABORT, 'unattributed legacy identity is protected'); END;
CREATE TRIGGER unattributed_protect_asset_people_INSERT BEFORE INSERT ON asset_people WHEN EXISTS(SELECT 1 FROM unattributed_legacy p WHERE p.library=NEW.library AND p.asset_id=NEW.asset_id) BEGIN SELECT RAISE(ABORT, 'unattributed legacy identity is protected'); END;
CREATE TRIGGER unattributed_protect_asset_people_UPDATE BEFORE UPDATE ON asset_people WHEN EXISTS(SELECT 1 FROM unattributed_legacy p WHERE p.library=OLD.library AND p.asset_id=OLD.asset_id) OR EXISTS(SELECT 1 FROM unattributed_legacy p WHERE p.library=NEW.library AND p.asset_id=NEW.asset_id) BEGIN SELECT RAISE(ABORT, 'unattributed legacy identity is protected'); END;
CREATE TRIGGER unattributed_protect_asset_people_DELETE BEFORE DELETE ON asset_people WHEN EXISTS(SELECT 1 FROM unattributed_legacy p WHERE p.library=OLD.library AND p.asset_id=OLD.asset_id) BEGIN SELECT RAISE(ABORT, 'unattributed legacy identity is protected'); END;
CREATE TRIGGER unattributed_protect_asset_album_memberships_INSERT BEFORE INSERT ON asset_album_memberships WHEN EXISTS(SELECT 1 FROM unattributed_legacy p WHERE p.library=NEW.library AND p.asset_id=NEW.asset_record_name) BEGIN SELECT RAISE(ABORT, 'unattributed legacy identity is protected'); END;
CREATE TRIGGER unattributed_protect_asset_album_memberships_UPDATE BEFORE UPDATE ON asset_album_memberships WHEN EXISTS(SELECT 1 FROM unattributed_legacy p WHERE p.library=OLD.library AND p.asset_id=OLD.asset_record_name) OR EXISTS(SELECT 1 FROM unattributed_legacy p WHERE p.library=NEW.library AND p.asset_id=NEW.asset_record_name) BEGIN SELECT RAISE(ABORT, 'unattributed legacy identity is protected'); END;
CREATE TRIGGER unattributed_protect_asset_album_memberships_DELETE BEFORE DELETE ON asset_album_memberships WHEN EXISTS(SELECT 1 FROM unattributed_legacy p WHERE p.library=OLD.library AND p.asset_id=OLD.asset_record_name) BEGIN SELECT RAISE(ABORT, 'unattributed legacy identity is protected'); END;
CREATE TRIGGER unattributed_protect_legacy_master_state_owners_INSERT BEFORE INSERT ON legacy_master_state_owners WHEN EXISTS(SELECT 1 FROM unattributed_legacy p WHERE p.library=NEW.library AND p.asset_id=NEW.master_record_name) BEGIN SELECT RAISE(ABORT, 'unattributed legacy identity is protected'); END;
CREATE TRIGGER unattributed_protect_legacy_master_state_owners_UPDATE BEFORE UPDATE ON legacy_master_state_owners WHEN EXISTS(SELECT 1 FROM unattributed_legacy p WHERE p.library=OLD.library AND p.asset_id=OLD.master_record_name) OR EXISTS(SELECT 1 FROM unattributed_legacy p WHERE p.library=NEW.library AND p.asset_id=NEW.master_record_name) BEGIN SELECT RAISE(ABORT, 'unattributed legacy identity is protected'); END;
CREATE TRIGGER unattributed_protect_legacy_master_state_owners_DELETE BEFORE DELETE ON legacy_master_state_owners WHEN EXISTS(SELECT 1 FROM unattributed_legacy p WHERE p.library=OLD.library AND p.asset_id=OLD.master_record_name) BEGIN SELECT RAISE(ABORT, 'unattributed legacy identity is protected'); END;
CREATE TABLE provider_shadow_pages (
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
CREATE TABLE provider_shadow_records (
    page_id INTEGER NOT NULL REFERENCES provider_shadow_pages(id),
    ordinal INTEGER NOT NULL CHECK(ordinal >= 0),
    record_name TEXT NOT NULL CHECK(length(trim(record_name)) > 0),
    record_type TEXT,
    deleted INTEGER NOT NULL CHECK(deleted IN (0,1)),
    PRIMARY KEY(page_id,ordinal)
) WITHOUT ROWID;
CREATE TABLE provider_shadow_receipts (
    scope TEXT PRIMARY KEY,
    page_id INTEGER NOT NULL REFERENCES provider_shadow_pages(id)
) WITHOUT ROWID;
CREATE TABLE provider_catalog_records (
    page_id INTEGER NOT NULL,
    ordinal INTEGER NOT NULL,
    record_name TEXT NOT NULL,
    record_type TEXT,
    deleted INTEGER NOT NULL CHECK(deleted IN (0,1)),
    kind TEXT NOT NULL CHECK(kind IN ('master','asset','album','relation','unknown')),
    PRIMARY KEY(page_id,ordinal),
    FOREIGN KEY(page_id,ordinal) REFERENCES provider_shadow_records(page_id,ordinal)
) WITHOUT ROWID;
CREATE TABLE provider_catalog_references (
    page_id INTEGER NOT NULL,
    ordinal INTEGER NOT NULL,
    field_path TEXT NOT NULL,
    target_record_name TEXT NOT NULL CHECK(length(trim(target_record_name))>0),
    target_zone_name TEXT,
    target_zone_owner TEXT,
    PRIMARY KEY(page_id,ordinal,field_path),
    FOREIGN KEY(page_id,ordinal) REFERENCES provider_catalog_records(page_id,ordinal)
) WITHOUT ROWID;
CREATE TABLE provider_catalog_debt (
    page_id INTEGER NOT NULL,
    ordinal INTEGER NOT NULL,
    field_path TEXT NOT NULL,
    reason TEXT NOT NULL,
    PRIMARY KEY(page_id,ordinal,field_path,reason),
    FOREIGN KEY(page_id,ordinal) REFERENCES provider_catalog_records(page_id,ordinal)
) WITHOUT ROWID;
CREATE TABLE provider_catalog_pages (
    page_id INTEGER PRIMARY KEY REFERENCES provider_shadow_pages(id),
    projector_version INTEGER NOT NULL CHECK(projector_version>0),
    body_hash TEXT NOT NULL,
    record_count INTEGER NOT NULL CHECK(record_count>=0),
    reference_count INTEGER NOT NULL CHECK(reference_count>=0),
    debt_count INTEGER NOT NULL CHECK(debt_count>=0),
    charged_bytes INTEGER NOT NULL CHECK(charged_bytes>0)
);
CREATE TABLE provider_work_receipts (
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
CREATE TABLE provider_work_obligations (
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
CREATE INDEX idx_provider_work_queue ON provider_work_obligations(library,asset_id,version_size);
CREATE TABLE provider_work_scan (
    scope TEXT NOT NULL,
    config_hash TEXT NOT NULL,
    page_id INTEGER NOT NULL,
    ordinal INTEGER NOT NULL,
    PRIMARY KEY(scope,config_hash)
) WITHOUT ROWID;
CREATE TABLE provider_work_retries (
    page_id INTEGER NOT NULL,
    ordinal INTEGER NOT NULL,
    config_hash TEXT NOT NULL,
    attempts INTEGER NOT NULL CHECK(attempts BETWEEN 1 AND 32),
    last_attempt_at INTEGER NOT NULL,
    next_retry_at INTEGER NOT NULL CHECK(next_retry_at > last_attempt_at),
    PRIMARY KEY(page_id,ordinal,config_hash),
    FOREIGN KEY(page_id,ordinal) REFERENCES provider_catalog_records(page_id,ordinal)
) WITHOUT ROWID;
CREATE TABLE provider_selection_generations (
    id TEXT PRIMARY KEY,
    account_key TEXT NOT NULL,
    provider_key TEXT NOT NULL,
    format INTEGER NOT NULL CHECK(format=1),
    scope TEXT NOT NULL,
    config_hash TEXT NOT NULL,
    manifest BLOB NOT NULL,
    charged_bytes INTEGER NOT NULL CHECK(charged_bytes>0)
) WITHOUT ROWID;
CREATE TABLE provider_selection_sources (
    generation TEXT NOT NULL REFERENCES provider_selection_generations(id),
    page_id INTEGER NOT NULL,
    ordinal INTEGER NOT NULL,
    body_hash TEXT NOT NULL,
    PRIMARY KEY(generation,page_id,ordinal),
    FOREIGN KEY(page_id,ordinal) REFERENCES provider_catalog_records(page_id,ordinal)
) WITHOUT ROWID;
CREATE TABLE provider_selection_decisions (
    generation TEXT NOT NULL REFERENCES provider_selection_generations(id),
    pass_key TEXT NOT NULL,
    child TEXT NOT NULL,
    outcome TEXT NOT NULL CHECK(outcome IN ('selected','excluded','deferred')),
    reason TEXT NOT NULL,
    PRIMARY KEY(generation,pass_key,child)
) WITHOUT ROWID;
CREATE TABLE provider_selection_destinations (
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
CREATE TABLE provider_active_generations (
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
CREATE TABLE provider_selection_rank_pages (
 id TEXT PRIMARY KEY, generation TEXT NOT NULL REFERENCES provider_active_generations(id),
 pass_key TEXT NOT NULL, request BLOB NOT NULL, request_hash TEXT NOT NULL,
 body BLOB NOT NULL, body_hash TEXT NOT NULL,
 charged_bytes INTEGER NOT NULL CHECK(charged_bytes>0), retired_proof_hash TEXT
                    CHECK(retired_proof_hash IS NULL OR length(retired_proof_hash)=64), has_relation INTEGER NOT NULL DEFAULT 0
                        CHECK(has_relation IN (0,1)),
 UNIQUE(generation,pass_key,request_hash,body_hash)
) WITHOUT ROWID;
CREATE TABLE provider_selection_rank_records (
 page_id TEXT NOT NULL REFERENCES provider_selection_rank_pages(id),
 ordinal INTEGER NOT NULL CHECK(ordinal>=0), record_name TEXT NOT NULL,
 record_type TEXT, deleted INTEGER NOT NULL CHECK(deleted IN (0,1)),
 PRIMARY KEY(page_id,ordinal)
) WITHOUT ROWID;
CREATE INDEX idx_selection_rank_identity ON provider_selection_rank_records(record_name,page_id);
CREATE TABLE provider_active_decisions (
 generation TEXT NOT NULL REFERENCES provider_active_generations(id), pass_key TEXT NOT NULL,
 child TEXT NOT NULL, manifest BLOB NOT NULL, manifest_hash TEXT NOT NULL,
 outcome TEXT NOT NULL CHECK(outcome IN ('selected','excluded','deferred')),
 admission TEXT NOT NULL CHECK(admission IN ('admitted','excluded','deferred')),
 reason TEXT NOT NULL, admission_hash TEXT, attempts INTEGER NOT NULL DEFAULT 0,
 next_retry_at INTEGER NOT NULL DEFAULT 0,
 charged_bytes INTEGER NOT NULL CHECK(charged_bytes>0),
 PRIMARY KEY(generation,pass_key,child)
) WITHOUT ROWID;
CREATE TABLE provider_active_sources (
 generation TEXT NOT NULL, pass_key TEXT NOT NULL, child TEXT NOT NULL,
 page_id TEXT NOT NULL, ordinal INTEGER NOT NULL,
 PRIMARY KEY(generation,pass_key,child,page_id,ordinal),
 FOREIGN KEY(generation,pass_key,child) REFERENCES provider_active_decisions(generation,pass_key,child),
 FOREIGN KEY(page_id,ordinal) REFERENCES provider_selection_rank_records(page_id,ordinal)
) WITHOUT ROWID;
CREATE TABLE provider_active_destinations (
 generation TEXT NOT NULL, pass_key TEXT NOT NULL, child TEXT NOT NULL,
 library TEXT NOT NULL, asset_id TEXT NOT NULL, master TEXT NOT NULL,
 version_size TEXT NOT NULL, path TEXT NOT NULL, compat_path TEXT NOT NULL,
 checksum TEXT NOT NULL, size_bytes INTEGER NOT NULL CHECK(size_bytes>=0),
 created_at REAL NOT NULL, added_at REAL, metadata_hash TEXT NOT NULL,
 admitted INTEGER NOT NULL CHECK(admitted IN (0,1)),
 verified_media INTEGER NOT NULL DEFAULT 0 CHECK(verified_media IN (0,1)),
 verified_metadata INTEGER NOT NULL DEFAULT 0 CHECK(verified_metadata IN (0,1)),
 local_checksum TEXT, source_checksum TEXT, grouping_hash TEXT, intent_hash TEXT, progress_hash TEXT,
 prepared_checksum TEXT, prepared_size INTEGER, prepared_hash TEXT, current_publication INTEGER NOT NULL DEFAULT 0 CHECK(current_publication IN (0,1)), preserved_local BLOB, embedded_metadata_receipt TEXT, confirmed_repair BLOB, writer_output_proof TEXT CHECK(writer_output_proof IS NULL OR length(writer_output_proof)=64), metadata_successor TEXT,
 CHECK((prepared_checksum IS NULL AND prepared_size IS NULL AND prepared_hash IS NULL)
    OR (prepared_checksum IS NOT NULL AND prepared_size IS NOT NULL AND prepared_hash IS NOT NULL
        AND length(prepared_checksum)=64 AND prepared_size>=0 AND length(prepared_hash)=64)),
 PRIMARY KEY(generation,pass_key,child,version_size,path),
 FOREIGN KEY(generation,pass_key,child) REFERENCES provider_active_decisions(generation,pass_key,child)
) WITHOUT ROWID;
CREATE INDEX idx_active_destination_identity ON provider_active_destinations(library,asset_id,version_size,admitted,verified_media,verified_metadata);
CREATE TABLE primary_layout_bindings (
 family TEXT PRIMARY KEY, library TEXT NOT NULL, source_library TEXT NOT NULL, child TEXT NOT NULL,
 generation INTEGER NOT NULL CHECK(generation > 0), decision TEXT NOT NULL,
 policy TEXT NOT NULL, binding BLOB NOT NULL
);
CREATE TABLE primary_layout_operations (
 operation TEXT PRIMARY KEY, family TEXT NOT NULL, library TEXT NOT NULL, source_library TEXT NOT NULL,
 prior_generation INTEGER NOT NULL, decision TEXT NOT NULL, policy TEXT NOT NULL,
 phase TEXT NOT NULL CHECK(phase IN ('planned','prepared','preserved','publishing','committed','conflict','cancelled')),
 header BLOB NOT NULL, conflict TEXT
);
CREATE UNIQUE INDEX primary_layout_one_pending ON primary_layout_operations(family) WHERE phase NOT IN ('committed','cancelled');
CREATE TABLE primary_layout_members (
 operation TEXT NOT NULL REFERENCES primary_layout_operations(operation),
 member INTEGER NOT NULL, evidence BLOB NOT NULL, PRIMARY KEY(operation,member)
);
CREATE TABLE primary_layout_claims (
 path_key TEXT PRIMARY KEY, family TEXT NOT NULL, operation TEXT,
 library TEXT NOT NULL, child TEXT NOT NULL, version TEXT NOT NULL,
 provider_checksum TEXT NOT NULL, local_checksum TEXT,
 native_path BLOB NOT NULL
);
CREATE TABLE primary_layout_preserved (
 operation TEXT NOT NULL, member INTEGER NOT NULL, native_path BLOB NOT NULL,
 media_checksum TEXT NOT NULL, sidecar_checksum TEXT, evidence BLOB NOT NULL,
 PRIMARY KEY(operation,member)
);
CREATE TABLE primary_layout_superseded_paths (
 library TEXT NOT NULL, child TEXT NOT NULL, version TEXT NOT NULL,
 provider_checksum TEXT NOT NULL, native_path BLOB NOT NULL, compat_path TEXT,
 PRIMARY KEY(library,child,version,provider_checksum,native_path)
);
CREATE INDEX primary_layout_superseded_compat ON primary_layout_superseded_paths(library,child,version,provider_checksum,compat_path);
CREATE INDEX primary_layout_binding_source ON primary_layout_bindings(source_library);
CREATE INDEX primary_layout_binding_owner ON primary_layout_bindings(library,child);
CREATE INDEX primary_layout_claim_owner ON primary_layout_claims(library,child);
CREATE INDEX primary_layout_pending_library ON primary_layout_operations(source_library,phase);
CREATE TABLE provider_transport_canonical (
    canonical_sha TEXT NOT NULL,
    version INTEGER NOT NULL CHECK(version > 0),
    page_id INTEGER NOT NULL REFERENCES provider_shadow_pages(id),
    PRIMARY KEY(canonical_sha, version)
) WITHOUT ROWID;
CREATE UNIQUE INDEX idx_transport_canonical_page
    ON provider_transport_canonical(page_id);
CREATE TABLE provider_transport_aliases (
    page_id INTEGER PRIMARY KEY REFERENCES provider_shadow_pages(id),
    canonical_page_id INTEGER NOT NULL REFERENCES provider_shadow_pages(id),
    classifier_version INTEGER NOT NULL CHECK(classifier_version > 0),
    masked_sha TEXT NOT NULL,
    certificate TEXT NOT NULL,
    provenance_hash TEXT NOT NULL,
    CHECK(page_id <> canonical_page_id)
) WITHOUT ROWID;
CREATE INDEX idx_transport_alias_canonical
    ON provider_transport_aliases(canonical_page_id);
CREATE UNIQUE INDEX idx_current_native_publication ON provider_active_destinations(library,path) WHERE current_publication=1;
CREATE TABLE provider_finished_changes (
                page_id INTEGER PRIMARY KEY REFERENCES provider_shadow_pages(id),
                receipt TEXT NOT NULL
            );
CREATE TABLE historical_deletion_checks (
    library TEXT NOT NULL, id TEXT NOT NULL, retry_at INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (library, id)
) WITHOUT ROWID;
CREATE INDEX historical_deletion_due ON historical_deletion_checks(library, retry_at, id);
CREATE TRIGGER historical_deletion_observed AFTER UPDATE OF last_seen_at, is_deleted ON assets
BEGIN
    DELETE FROM historical_deletion_checks WHERE library=NEW.library AND id=NEW.id;
END;
CREATE TRIGGER historical_deletion_inserted AFTER INSERT ON assets
BEGIN
    DELETE FROM historical_deletion_checks WHERE library=NEW.library AND id=NEW.id;
END;
CREATE INDEX idx_selection_rank_unretired ON provider_selection_rank_pages(generation)
                 WHERE retired_proof_hash IS NULL;
CREATE INDEX idx_verified_writer_output ON provider_active_destinations(library,asset_id,path,writer_output_proof) WHERE writer_output_proof IS NOT NULL;
CREATE INDEX idx_selection_rank_relation_pages
                   ON provider_selection_rank_pages(generation,pass_key)
                   WHERE has_relation=1;
CREATE INDEX idx_active_destination_child
                   ON provider_active_destinations(child,library);
