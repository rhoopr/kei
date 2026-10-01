-- Schema emitted by the official v0.24.0 Linux x86_64 binary. No user data.
BEGIN TRANSACTION;
CREATE TABLE album_containers (
    library TEXT NOT NULL,
    container_id TEXT NOT NULL,
    album_name TEXT NOT NULL,
    pass_kind TEXT NOT NULL,
    is_deleted INTEGER NOT NULL DEFAULT 0,
    updated_at INTEGER NOT NULL,
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
CREATE TABLE "asset_albums" (
    library    TEXT NOT NULL,
    asset_id   TEXT NOT NULL,
    album_name TEXT NOT NULL,
    source     TEXT NOT NULL,
    PRIMARY KEY (library, asset_id, album_name, source)
);
CREATE TABLE asset_master_mappings (
    library TEXT NOT NULL,
    asset_record_name TEXT NOT NULL,
    master_record_name TEXT NOT NULL,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (library, asset_record_name)
);
CREATE TABLE asset_metadata_capture_revisions (
    library TEXT NOT NULL,
    asset_id TEXT NOT NULL,
    revision INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (library, asset_id)
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
    capture_repair_output_size INTEGER, source_checksum TEXT,
    PRIMARY KEY (library, id, version_size, local_path)
);
CREATE TABLE "asset_people" (
    library     TEXT NOT NULL,
    asset_id    TEXT NOT NULL,
    person_name TEXT NOT NULL,
    PRIMARY KEY (library, asset_id, person_name)
);
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
CREATE TABLE legacy_master_state_owners (
    library TEXT NOT NULL,
    master_record_name TEXT NOT NULL,
    asset_record_name TEXT NOT NULL,
    claimed_at INTEGER NOT NULL,
    PRIMARY KEY (library, master_record_name)
);
CREATE TABLE metadata (
    key TEXT PRIMARY KEY NOT NULL,
    value TEXT NOT NULL
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
CREATE TABLE owned_temp_files (
    path BLOB PRIMARY KEY NOT NULL,
    claimed_at INTEGER NOT NULL
);
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
CREATE TABLE sync_runs (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    started_at INTEGER NOT NULL,
    completed_at INTEGER,
    assets_seen INTEGER DEFAULT 0,
    assets_downloaded INTEGER DEFAULT 0,
    assets_failed INTEGER DEFAULT 0,
    interrupted INTEGER DEFAULT 0
, status TEXT NOT NULL DEFAULT 'running', enumeration_errors INTEGER NOT NULL DEFAULT 0, api_total_at_start INTEGER, api_total_at_start_partial INTEGER NOT NULL DEFAULT 0, inventory_drop_detected INTEGER NOT NULL DEFAULT 0, inventory_drop_previous_total INTEGER, inventory_drop_current_total INTEGER, inventory_drop_library TEXT);
CREATE INDEX idx_assets_status ON assets(status);
CREATE INDEX idx_assets_local_path ON assets(local_path);
CREATE INDEX idx_assets_checksum ON assets(checksum);
CREATE INDEX idx_assets_metadata_hash ON assets (metadata_hash) WHERE status = 'downloaded';
CREATE INDEX idx_asset_albums_lookup
    ON asset_albums (library, asset_id);
CREATE INDEX idx_asset_people_lookup
    ON asset_people (library, asset_id);
CREATE INDEX idx_album_containers_lookup
    ON album_containers (library, album_name);
CREATE INDEX idx_album_membership_snapshots_status
    ON album_membership_snapshots (library, container_id, status);
CREATE INDEX idx_asset_album_memberships_asset
    ON asset_album_memberships (library, asset_record_name, is_deleted);
CREATE INDEX idx_asset_album_memberships_container
    ON asset_album_memberships (library, container_id, is_deleted);
CREATE INDEX idx_asset_master_mappings_master
    ON asset_master_mappings (library, master_record_name);
CREATE INDEX idx_asset_verifications_state
    ON asset_verifications (state, checked_at);
CREATE INDEX idx_asset_metadata_capture_revision
    ON asset_metadata_capture_revisions (library, revision);
CREATE INDEX idx_asset_metadata_paths_retry
    ON asset_metadata_paths(metadata_write_failed_at, library, id, version_size, local_path)
    WHERE metadata_write_failed_at IS NOT NULL;
CREATE INDEX idx_legacy_master_state_owners_asset ON legacy_master_state_owners (library, asset_record_name, master_record_name);
CREATE INDEX idx_reconciliation_paths_destination ON reconciliation_paths(destination_path_key);
DELETE FROM "sqlite_sequence";
COMMIT;
PRAGMA user_version = 25;
