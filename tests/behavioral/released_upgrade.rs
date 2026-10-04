//! Reconstructed history, not captured reporter data or a simulated provider sync.
use std::path::{Path, PathBuf};

use base64::Engine as _;
use rusqlite::Connection;
use sha2::{Digest, Sha256};

const SCHEMA: &str = include_str!("../data/released-v0240-schema.sql");

fn run(binary: &Path, root: &Path, args: &[&str], success: bool) -> String {
    let mut command = assert_cmd::Command::new(binary);
    let output = command
        .env_clear()
        .env("ICLOUD_USERNAME", "upgrade@example.invalid")
        .env("KEI_DATA_DIR", root)
        .arg("--config")
        .arg(root.join("config.toml"))
        .args(args)
        .timeout(std::time::Duration::from_secs(15))
        .output()
        .unwrap();
    assert_eq!(
        output.status.success(),
        success,
        "command {binary:?} {args:?}: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap() + &String::from_utf8(output.stderr).unwrap()
}

fn rows(db: &Path, sql: &str) -> Vec<Vec<rusqlite::types::Value>> {
    let conn = Connection::open(db).unwrap();
    let mut stmt = conn.prepare(sql).unwrap();
    let columns = stmt.column_count();
    stmt.query_map([], |row| (0..columns).map(|i| row.get(i)).collect())
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

fn schema_version(db: &Path) -> i32 {
    Connection::open(db)
        .unwrap()
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap()
}

/// Default gate uses the released schema fixture. The optional qualification
/// script supplies a checksum-verified old binary and runs this same history
/// inside a Linux network namespace. No release is downloaded by this test.
#[test]
fn released_v0240_history_preserves_durable_evidence_through_upgrade() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let db = root.join("upgradeexampleinvalid.db");
    let config = root.join("config.toml");
    let current = PathBuf::from(assert_cmd::cargo::cargo_bin!("kei"));
    super::write_sync_config(&config, root.join("media").to_str().unwrap());
    let old = std::env::var_os("KEI_TEST_RELEASED_V0240").map(PathBuf::from);
    if let Some(old) = &old {
        // Empty file only: the released production migration creates every table.
        std::fs::write(&db, []).unwrap();
        run(old, root, &["verify"], true);
        assert_eq!(schema_version(&db), 25);
        let expected = Connection::open_in_memory().unwrap();
        expected.execute_batch(SCHEMA).unwrap();
        let actual = Connection::open(&db).unwrap();
        let schema = |conn: &Connection| {
            conn.prepare("SELECT type,name,tbl_name,sql FROM sqlite_master WHERE name NOT LIKE 'sqlite_%' ORDER BY type,name")
                .unwrap()
                .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?)))
                .unwrap().collect::<Result<Vec<_>, _>>().unwrap()
        };
        assert_eq!(schema(&actual), schema(&expected), "released schema drift");
    } else {
        Connection::open(&db)
            .unwrap()
            .execute_batch(SCHEMA)
            .unwrap();
    }
    assert_eq!(schema_version(&db), 25);
    let media = root.join("media");
    std::fs::create_dir(&media).unwrap();
    let original = b"synthetic original bytes; never a reporter photo";
    let edited = b"synthetic edited rendition";
    let shared = b"synthetic shared-library bytes";
    let removed = b"synthetic locally removed bytes";
    let entries = [
        (
            "PrimarySync",
            "same",
            "original",
            "original.jpg",
            original.as_slice(),
        ),
        (
            "PrimarySync",
            "same",
            "adjusted",
            "edited.jpg",
            edited.as_slice(),
        ),
        (
            "SharedSync-synthetic",
            "same",
            "original",
            "shared.jpg",
            shared.as_slice(),
        ),
        (
            "PrimarySync",
            "removed",
            "original",
            "removed.jpg",
            removed.as_slice(),
        ),
    ];
    let conn = Connection::open(&db).unwrap();
    for (library, id, version, filename, bytes) in entries {
        let path = media.join(filename);
        std::fs::write(&path, bytes).unwrap();
        let checksum = format!("{:x}", Sha256::digest(bytes));
        let provider_checksum =
            base64::engine::general_purpose::STANDARD.encode(Sha256::digest(bytes));
        conn.execute("INSERT INTO assets(library,id,version_size,checksum,filename,created_at,size_bytes,media_type,status,local_path,last_seen_at,local_checksum,download_checksum) VALUES (?1,?2,?3,?8,?5,1000,?6,'photo','downloaded',?7,1000,?4,?4)",
            rusqlite::params![library,id,version,checksum,filename,bytes.len() as i64,path.to_str().unwrap(),provider_checksum]).unwrap();
    }
    // Deliberate fixture SQL models already-persisted provider facts and an
    // interrupted previous cycle. It is not attributed to the released CLI.
    conn.execute_batch("
        INSERT INTO asset_master_mappings VALUES ('PrimarySync','same','master-primary',1000),('SharedSync-synthetic','same','master-shared',1000);
        INSERT INTO legacy_master_state_owners VALUES ('PrimarySync','master-primary','same',1000);
        INSERT INTO asset_albums VALUES ('PrimarySync','same','Original album','icloud'),('SharedSync-synthetic','same','Shared album','icloud');
        INSERT INTO metadata VALUES ('sync_token:PrimarySync','committed-before-interruption'),('pending_sync_token:old-enum:PrimarySync','uncommitted-tail'),('config_hash','old-config'),('enum_config_hash','old-enum');
        INSERT INTO metadata_capture_state(library,pending_revision,failed_assets,updated_at) VALUES ('PrimarySync',1,1,1000);
        INSERT INTO sync_runs(started_at,status) VALUES (1000,'running');
        INSERT INTO assets(library,id,version_size,checksum,filename,created_at,size_bytes,media_type,status,last_seen_at,last_error) VALUES ('PrimarySync','pending','original','pending-checksum','pending.jpg',1000,100,'photo','pending',1000,'interrupted transfer');
        UPDATE assets SET title='edited title',metadata_write_failed_at=1001 WHERE library='PrimarySync' AND id='same' AND version_size='adjusted';
    ").unwrap();
    drop(conn);
    let part = media.join("pending.jpg.part");
    std::fs::write(&part, b"synthetic partial transfer").unwrap();
    let before = rows(&db, "SELECT * FROM assets ORDER BY library,id,version_size");
    let identities = rows(&db, "SELECT * FROM asset_master_mappings ORDER BY library");
    let owners = rows(
        &db,
        "SELECT * FROM legacy_master_state_owners ORDER BY library",
    );
    let checkpoints = rows(&db, "SELECT * FROM metadata ORDER BY key");
    let capture = rows(&db, "SELECT * FROM metadata_capture_state ORDER BY library");
    let runs = rows(&db, "SELECT * FROM sync_runs ORDER BY id");
    let albums = rows(&db, "SELECT * FROM asset_albums ORDER BY library");

    // Meaningful old CLI validation before the upgrade, including detecting
    // external edits without acknowledging them as downloaded bytes.
    if let Some(old) = &old {
        assert!(run(old, root, &["verify", "--checksums"], true).contains("Verified:  4"));
        std::fs::write(media.join("edited.jpg"), b"external edit").unwrap();
        assert!(run(old, root, &["verify", "--checksums"], false).contains("CORRUPTED:"));
        std::fs::write(media.join("edited.jpg"), edited).unwrap();
        assert_eq!(
            rows(&db, "SELECT * FROM assets ORDER BY library,id,version_size"),
            before
        );
    }
    // Unowned released state is never silently adopted by an offline command.
    let refusal = run(&current, root, &["verify"], false);
    assert!(refusal.contains("explicit verified ownership migration"));
    assert_eq!(schema_version(&db), 25);
    // Synthetic setup models the post-confirmation owner header only. The
    // real SQLite backup/adoption transition is tested in state::db::account.
    let legacy = db;
    let db = root.join(format!(
        "{}.db",
        super::support::sanitize_username("upgrade@example.invalid")
    ));
    std::fs::copy(&legacy, &db).unwrap();
    let adopted = Connection::open(&db).unwrap();
    super::support::bind_synthetic_owner(&adopted, "upgrade@example.invalid");
    drop(adopted);
    std::fs::remove_file(media.join("removed.jpg")).unwrap();
    // Config drift is not permission for an offline command to advance tokens.
    super::write_sync_config(&config, root.join("new-destination").to_str().unwrap());
    let verification = run(&current, root, &["verify", "--checksums"], false);
    assert!(verification.contains("Missing:   1"), "{verification}");
    assert_eq!(schema_version(&db), 29);
    assert_eq!(
        rows(&db, "SELECT * FROM assets ORDER BY library,id,version_size"),
        before
    );
    assert_eq!(
        rows(&db, "SELECT * FROM asset_master_mappings ORDER BY library"),
        identities
    );
    assert_eq!(
        rows(
            &db,
            "SELECT * FROM legacy_master_state_owners ORDER BY library"
        ),
        owners
    );
    assert_eq!(
        rows(&db, "SELECT * FROM metadata ORDER BY key"),
        checkpoints
    );
    assert_eq!(
        rows(&db, "SELECT * FROM metadata_capture_state ORDER BY library"),
        capture
    );
    assert_eq!(rows(&db, "SELECT * FROM sync_runs ORDER BY id"), runs);
    assert_eq!(
        rows(&db, "SELECT * FROM asset_albums ORDER BY library"),
        albums
    );

    // Actual production reconciliation durably enqueues missing media. It does
    // not delete the row, grant identity, or pretend restoration is a download.
    run(&current, root, &["reconcile"], true);
    let failed = rows(
        &db,
        "SELECT status,last_error FROM assets WHERE id='removed'",
    );
    assert_eq!(
        failed,
        vec![vec![
            "failed".to_string().into(),
            "FILE_MISSING_AT_STARTUP".to_string().into()
        ]]
    );
    std::fs::write(media.join("removed.jpg"), removed).unwrap();
    let stable = rows(&db, "SELECT * FROM assets ORDER BY library,id,version_size");
    for _ in 0..2 {
        run(&current, root, &["reconcile"], true);
        run(&current, root, &["verify", "--checksums"], true);
        assert_eq!(
            rows(&db, "SELECT * FROM assets ORDER BY library,id,version_size"),
            stable
        );
        assert_eq!(
            rows(&db, "SELECT * FROM metadata ORDER BY key"),
            checkpoints
        );
        assert_eq!(schema_version(&db), 29);
        for (_, _, _, filename, bytes) in entries {
            assert_eq!(std::fs::read(media.join(filename)).unwrap(), bytes);
        }
        assert_eq!(std::fs::read(&part).unwrap(), b"synthetic partial transfer");
    }
}
