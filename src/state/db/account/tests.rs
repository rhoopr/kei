use super::{AccountOwner, adopt_legacy, state_path};
use crate::state::{SqliteStateDb, error::StateError};

// A separate test process exits without dropping its connection, leaving the
// same WAL/SHM state as an interrupted process. This uses no external runtime.
#[test]
fn crashed_legacy_fixture_child() {
    let Some(path) = std::env::var_os("KEI_SYNTHETIC_CRASHED_DB") else {
        return;
    };
    let conn = rusqlite::Connection::open(path).unwrap();
    conn.pragma_update(None, "journal_mode", "WAL").unwrap();
    crate::state::schema::migrate(&conn).unwrap();
    conn.execute("INSERT INTO metadata(key,value) VALUES ('sync_token:PrimarySync','crashed-retained'), ('pending_sync_token:epoch:PrimarySync','crashed-debt')", []).unwrap();
    // Intentionally bypass destructors only in the isolated fixture process.
    std::process::exit(0);
}

fn crashed_legacy_fixture(path: &std::path::Path) {
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "state::db::account::tests::crashed_legacy_fixture_child",
            "--exact",
        ])
        .env("KEI_SYNTHETIC_CRASHED_DB", path)
        .status()
        .unwrap();
    assert!(status.success());
    assert!(path.with_extension("db-wal").exists());
    assert!(path.with_extension("db-shm").exists());
}

#[tokio::test]
async fn ownership_refusal_preserves_crash_left_database_and_wal() {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("crashed.db");
    crashed_legacy_fixture(&source);
    let before = std::fs::read(&source).unwrap();
    let wal = source.with_extension("db-wal");
    let wal_before = std::fs::read(&wal).unwrap();
    assert!(matches!(
        SqliteStateDb::open_owned(
            &source,
            &owner("synthetic@example.invalid", "com", "provider")
        )
        .await,
        Err(StateError::AccountOwnerMissing)
    ));
    assert!(
        before == std::fs::read(&source).unwrap(),
        "legacy database bytes changed"
    );
    assert!(
        wal_before == std::fs::read(&wal).unwrap(),
        "legacy WAL bytes changed"
    );
    assert!(source.with_extension("db-shm").exists());
}

#[tokio::test]
async fn crash_left_adoption_survives_failed_publication_and_retry() {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("crashed.db");
    crashed_legacy_fixture(&source);
    let before = std::fs::read(&source).unwrap();
    let wal = source.with_extension("db-wal");
    let wal_before = std::fs::read(&wal).unwrap();
    let destination = directory.path().join("target.db");
    std::fs::create_dir(&destination).unwrap();
    let expected = owner("synthetic@example.invalid", "com", "provider");
    assert!(
        adopt_legacy(
            &source,
            &destination,
            &expected,
            "synthetic@example.invalid",
            "com"
        )
        .await
        .is_err()
    );
    assert!(destination.is_dir());
    std::fs::remove_dir(&destination).unwrap();
    adopt_legacy(
        &source,
        &destination,
        &expected,
        "synthetic@example.invalid",
        "com",
    )
    .await
    .unwrap();
    assert!(
        before == std::fs::read(&source).unwrap(),
        "legacy database bytes changed"
    );
    assert!(
        wal_before == std::fs::read(&wal).unwrap(),
        "legacy WAL bytes changed"
    );
    assert!(source.with_extension("db-shm").exists());
    for _ in 0..2 {
        let db = SqliteStateDb::open_owned(&destination, &expected)
            .await
            .unwrap();
        assert_eq!(
            db.get_metadata("sync_token:PrimarySync")
                .await
                .unwrap()
                .as_deref(),
            Some("crashed-retained")
        );
        assert_eq!(
            db.get_metadata("pending_sync_token:epoch:PrimarySync")
                .await
                .unwrap()
                .as_deref(),
            Some("crashed-debt")
        );
    }
}

fn owner(username: &str, realm: &str, dsid: &str) -> AccountOwner {
    AccountOwner::authenticated(
        username,
        realm,
        &serde_json::from_value(serde_json::json!({"dsInfo":{"dsid":dsid}})).unwrap(),
    )
    .unwrap()
}

#[tokio::test]
async fn owned_database_refuses_wrong_account_provider_realm_and_unbound_files() {
    let directory = tempfile::tempdir().unwrap();
    let a = "first.last@example.invalid";
    let b = "firstlast@example.invalid";
    let path = state_path(directory.path(), a, "com").await.unwrap();
    let db = SqliteStateDb::open_owned(&path, &owner(a, "com", "provider-a"))
        .await
        .unwrap();
    db.set_metadata("sync_token:PrimarySync", "retained")
        .await
        .unwrap();
    drop(db);
    assert_ne!(path, state_path(directory.path(), b, "com").await.unwrap());
    for expectation in [
        owner(b, "com", "provider-a"),
        owner(a, "cn", "provider-a"),
        owner(a, "com", "provider-b"),
    ] {
        assert!(matches!(
            SqliteStateDb::open_owned(&path, &expectation).await,
            Err(StateError::AccountOwnerMismatch)
        ));
        assert!(matches!(
            SqliteStateDb::open_owned_read_only(&path, &expectation).await,
            Err(StateError::AccountOwnerMismatch)
        ));
    }
    let db = SqliteStateDb::open_owned_read_only(&path, &AccountOwner::configured(a, "com"))
        .await
        .unwrap();
    assert_eq!(
        db.get_metadata("sync_token:PrimarySync")
            .await
            .unwrap()
            .as_deref(),
        Some("retained")
    );
    drop(db);
    let unknown = directory.path().join("unbound.db");
    let legacy = SqliteStateDb::open(&unknown).await.unwrap();
    drop(legacy);
    let before = std::fs::read(&unknown).unwrap();
    assert!(matches!(
        SqliteStateDb::open_owned(&unknown, &owner(a, "com", "provider-a")).await,
        Err(StateError::AccountOwnerMissing)
    ));
    assert_eq!(before, std::fs::read(&unknown).unwrap());
    let empty = directory.path().join("interrupted-create.db");
    std::fs::write(&empty, []).unwrap();
    assert!(matches!(
        SqliteStateDb::open_owned(&empty, &owner(a, "com", "provider-a")).await,
        Err(StateError::AccountOwnerMissing)
    ));
    assert_eq!(std::fs::metadata(empty).unwrap().len(), 0);
}

#[tokio::test]
async fn explicit_adoption_preserves_live_wal_rows_unknowns_debt_and_cursors_then_reopens() {
    let directory = tempfile::tempdir().unwrap();
    let username = "first.last@example.invalid";
    let source = directory.path().join("firstlastexampleinvalid.db");
    let db = SqliteStateDb::open(&source).await.unwrap();
    db.set_metadata("sync_token:PrimarySync", "before-adoption")
        .await
        .unwrap();
    db.set_metadata("pending_sync_token:epoch:PrimarySync", "debt")
        .await
        .unwrap();
    {
        let conn = db.acquire_lock("synthetic adoption state").unwrap();
        conn.execute_batch("CREATE TABLE future_unknown(payload BLOB NOT NULL); INSERT INTO future_unknown VALUES (X'00FF'); INSERT INTO unresolved_sparse_identities(library,source_record_name,original_evidence,observed_evidence,generation,first_seen_at) VALUES ('PrimarySync','unknown-source','unknown-original','unknown-current',1,1000); INSERT INTO assets(library,id,version_size,checksum,filename,created_at,size_bytes,media_type,last_seen_at,status) VALUES ('PrimarySync','historical','original','synthetic-checksum','historical.jpg',1000,4,'photo',1000,'downloaded'),('PrimarySync','pending','original','pending-checksum','pending.jpg',1000,4,'photo',1000,'pending');").unwrap();
    }
    let bytes = std::fs::read(&source).unwrap();
    let wal = source.with_extension("db-wal");
    let wal_bytes = std::fs::read(&wal).unwrap();
    assert!(matches!(
        state_path(directory.path(), username, "com").await,
        Err(StateError::LegacyAccountMigrationRequired)
    ));
    let destination = directory
        .path()
        .join(format!("{}.db", crate::account::namespace(username, "com")));
    let expected = owner(username, "com", "synthetic-provider");
    adopt_legacy(&source, &destination, &expected, username, "com")
        .await
        .unwrap();
    assert_eq!(bytes, std::fs::read(&source).unwrap());
    assert_eq!(wal_bytes, std::fs::read(&wal).unwrap());
    for _ in 0..2 {
        let adopted = SqliteStateDb::open_owned(&destination, &expected)
            .await
            .unwrap();
        assert_eq!(
            adopted
                .get_metadata("sync_token:PrimarySync")
                .await
                .unwrap()
                .as_deref(),
            Some("before-adoption")
        );
        assert_eq!(
            adopted
                .get_metadata("pending_sync_token:epoch:PrimarySync")
                .await
                .unwrap()
                .as_deref(),
            Some("debt")
        );
        let conn = adopted.acquire_lock("verify copied state").unwrap();
        assert_eq!(
            conn.query_row("SELECT count(*) FROM assets", [], |row| row
                .get::<_, i64>(0))
                .unwrap(),
            2
        );
        assert_eq!(
            conn.query_row("SELECT payload FROM future_unknown", [], |row| row
                .get::<_, Vec<u8>>(0))
                .unwrap(),
            [0, 255]
        );
        assert_eq!(
            conn.query_row(
                "SELECT generation FROM unresolved_sparse_identities",
                [],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
            1
        );
        drop(conn);
        drop(adopted);
    }
    let published = std::fs::read(&destination).unwrap();
    assert!(
        adopt_legacy(&source, &destination, &expected, username, "com")
            .await
            .is_err()
    );
    assert_eq!(published, std::fs::read(&destination).unwrap());
    assert_eq!(bytes, std::fs::read(&source).unwrap());
    assert!(!std::fs::read_dir(directory.path()).unwrap().any(|entry| {
        entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".account-adoption-")
    }));
}

#[tokio::test]
async fn explicit_adoption_rejects_conflicting_legacy_provenance_and_unauthenticated_owner() {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("source.db");
    let db = SqliteStateDb::open(&source).await.unwrap();
    {
        let conn = db.acquire_lock("conflicting legacy scope").unwrap();
        conn.execute("INSERT INTO scoped_db_sync_tokens VALUES ('icloud','other@example.invalid',1,'scope','[]','{}','retained',1,1)", []).unwrap();
    }
    let destination = directory.path().join("target.db");
    assert!(matches!(
        adopt_legacy(
            &source,
            &destination,
            &owner("selected@example.invalid", "com", "provider"),
            "selected@example.invalid",
            "com"
        )
        .await,
        Err(StateError::AccountOwnerMismatch)
    ));
    assert!(!destination.exists());
    {
        let conn = db
            .acquire_lock("remove synthetic conflicting scope")
            .unwrap();
        conn.execute("DELETE FROM scoped_db_sync_tokens", [])
            .unwrap();
    }
    assert!(matches!(
        adopt_legacy(
            &source,
            &destination,
            &AccountOwner::configured("selected@example.invalid", "com"),
            "selected@example.invalid",
            "com"
        )
        .await,
        Err(StateError::AccountIdentityUnavailable)
    ));
    assert!(!destination.exists());
}

#[tokio::test]
async fn state_commands_refuse_misplaced_owned_database_before_reset_or_export() {
    let directory = tempfile::tempdir().unwrap();
    let a = "first.last@example.invalid";
    let b = "firstlast@example.invalid";
    let path_a = state_path(directory.path(), a, "com").await.unwrap();
    let db = SqliteStateDb::open_owned(&path_a, &owner(a, "com", "provider-a"))
        .await
        .unwrap();
    db.set_metadata("sync_token:PrimarySync", "retained")
        .await
        .unwrap();
    drop(db);
    let path_b = state_path(directory.path(), b, "com").await.unwrap();
    std::fs::copy(&path_a, &path_b).unwrap();
    let before = std::fs::read(&path_b).unwrap();
    let globals = crate::config::GlobalArgs {
        username: Some(b.to_string()),
        domain: None,
        data_dir: Some(directory.path().to_str().unwrap().to_string()),
    };
    assert!(
        crate::commands::run_status(
            crate::cli::StatusArgs {
                failed: true,
                pending: true,
                downloaded: true
            },
            &globals,
            None
        )
        .await
        .is_err()
    );
    assert!(
        crate::commands::run_verify(crate::cli::VerifyArgs { checksums: false }, &globals, None)
            .await
            .is_err()
    );
    assert!(
        crate::commands::reconcile::run_reconcile(
            crate::cli::ReconcileArgs { dry_run: false },
            &globals,
            None
        )
        .await
        .is_err()
    );
    assert!(
        crate::commands::run_reset_state(true, &globals, None)
            .await
            .is_err()
    );
    assert!(
        crate::commands::run_reset_sync_token(true, &globals, None, crate::InputMode::NoInput)
            .await
            .is_err()
    );
    assert!(
        crate::commands::run_manifest(
            crate::cli::ManifestArgs {
                format: crate::cli::ManifestFormat::Json
            },
            &globals,
            None
        )
        .await
        .is_err()
    );
    assert_eq!(before, std::fs::read(&path_b).unwrap());
    assert!(path_a.exists());
}

#[cfg(unix)]
#[tokio::test]
async fn unresolved_legacy_symlink_is_not_permission_for_new_state() {
    let directory = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink(
        directory.path().join("missing-source"),
        directory.path().join("firstlastexampleinvalid.db"),
    )
    .unwrap();
    assert!(matches!(
        state_path(directory.path(), "first.last@example.invalid", "com").await,
        Err(StateError::LegacyAccountMigrationRequired)
    ));
}
