use std::path::Path;
use std::time::Instant;

use rusqlite::{Connection, params};

use super::{key, path_is_protected, protected_paths};

fn database() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    crate::state::schema::migrate(&conn).unwrap();
    conn
}

fn asset(conn: &Connection, id: &str, path: &str) {
    conn.execute(
        "INSERT INTO assets(library,id,version_size,checksum,filename,created_at,size_bytes,
            media_type,last_seen_at,local_path) VALUES ('lib',?1,'original','hash','file',0,1,'photo',0,?2)",
        params![id, path],
    ).unwrap();
}

fn mappings(conn: &Connection, master: &str) {
    for child in ["a", "b"] {
        conn.execute(
            "INSERT INTO asset_master_mappings VALUES ('lib',?1,?2,0)",
            params![format!("{master}-{child}"), master],
        )
        .unwrap();
    }
}

fn guarded(conn: &Connection, path: &str, protected: bool) {
    for _ in 0..2 {
        assert_eq!(
            path_is_protected(conn, Path::new(path)).unwrap(),
            protected,
            "{path}"
        );
        assert_eq!(
            super::super::guard_writer(conn, Path::new(path)).is_err(),
            protected
        );
    }
}

fn forbid_rebuild(conn: &Connection) {
    conn.execute_batch(
        "CREATE TEMP TRIGGER reject_cache_rebuild BEFORE DELETE ON legacy_path_cache
            BEGIN SELECT RAISE(ABORT,'unexpected cache rebuild'); END;",
    )
    .unwrap();
}

#[test]
fn legacy_path_cache_confined_relative_absolute_dot_and_sidecar_paths() {
    let conn = database();
    asset(&conn, "master", "relative/./original.jpg");
    mappings(&conn, "master");
    guarded(&conn, "relative/original.jpg", true);
    guarded(&conn, "relative/./original.jpg.xmp", true);
    let absolute = std::env::current_dir()
        .unwrap()
        .join("relative/original.jpg");
    assert!(path_is_protected(&conn, &absolute).unwrap());
    conn.execute(
        "UPDATE temp.legacy_path_cache_valid SET cwd='stale-cwd'",
        [],
    )
    .unwrap();
    conn.execute("DELETE FROM temp.legacy_path_cache", [])
        .unwrap();
    // A cwd mismatch must rebuild normalized relative keys, even without a data change.
    assert!(path_is_protected(&conn, &absolute).unwrap());
    guarded(&conn, "relative/original.jpg.xmp.xmp", false);
    assert!(
        protected_paths(&conn)
            .unwrap()
            .contains(&"relative/./original.jpg".into())
    );
    assert!(path_is_protected(&conn, Path::new("relative/../original.jpg")).is_err());
    conn.execute("UPDATE assets SET local_path='relative/../invalid.jpg'", [])
        .unwrap();
    assert!(path_is_protected(&conn, Path::new("ordinary.jpg")).is_err());
}

#[test]
fn legacy_path_cache_mapping_owner_and_path_transitions() {
    let conn = database();
    asset(&conn, "master", "original.jpg");
    guarded(&conn, "original.jpg", false);
    conn.execute_batch("INSERT INTO asset_master_mappings VALUES ('lib','a','master',0)")
        .unwrap();
    guarded(&conn, "original.jpg", false);
    conn.execute_batch("INSERT INTO asset_master_mappings VALUES ('lib','b','master',0)")
        .unwrap();
    guarded(&conn, "original.jpg", true);
    conn.execute_batch("DELETE FROM asset_master_mappings WHERE asset_record_name='b'")
        .unwrap();
    guarded(&conn, "original.jpg", false);
    conn.execute_batch("INSERT INTO asset_master_mappings VALUES ('lib','b','elsewhere',0)")
        .unwrap();
    guarded(&conn, "original.jpg", false);
    conn.execute_batch(
        "UPDATE asset_master_mappings SET master_record_name='master' WHERE asset_record_name='b'",
    )
    .unwrap();
    guarded(&conn, "original.jpg", true);
    conn.execute_batch("INSERT INTO asset_master_mappings VALUES ('lib','master','parent',0)")
        .unwrap();
    guarded(&conn, "original.jpg", false);
    conn.execute_batch("UPDATE asset_master_mappings SET asset_record_name='other-child' WHERE asset_record_name='master'").unwrap();
    guarded(&conn, "original.jpg", true);
    conn.execute_batch("INSERT INTO asset_master_mappings VALUES ('lib','master','parent',0)")
        .unwrap();
    guarded(&conn, "original.jpg", false);
    conn.execute_batch("DELETE FROM asset_master_mappings WHERE asset_record_name='master'")
        .unwrap();
    guarded(&conn, "original.jpg", true);
    conn.execute_batch(
        "UPDATE asset_master_mappings SET library='other' WHERE asset_record_name='b'",
    )
    .unwrap();
    guarded(&conn, "original.jpg", false);
    conn.execute_batch(
        "UPDATE asset_master_mappings SET library='lib' WHERE asset_record_name='b'",
    )
    .unwrap();
    guarded(&conn, "original.jpg", true);
    conn.execute_batch("INSERT INTO legacy_master_state_owners VALUES ('lib','master','a',0)")
        .unwrap();
    guarded(&conn, "original.jpg", false);
    conn.execute_batch("UPDATE legacy_master_state_owners SET master_record_name='elsewhere'")
        .unwrap();
    guarded(&conn, "original.jpg", true);
    conn.execute_batch("UPDATE legacy_master_state_owners SET master_record_name='master'")
        .unwrap();
    guarded(&conn, "original.jpg", false);
    conn.execute_batch("DELETE FROM legacy_master_state_owners")
        .unwrap();
    guarded(&conn, "original.jpg", true);
    conn.execute_batch("UPDATE assets SET local_path='moved.jpg'")
        .unwrap();
    guarded(&conn, "original.jpg", false);
    guarded(&conn, "moved.jpg", true);
    conn.execute_batch("DELETE FROM assets").unwrap();
    guarded(&conn, "moved.jpg", false);
    asset(&conn, "master", "returned.jpg");
    guarded(&conn, "returned.jpg", true);
    conn.execute_batch("UPDATE assets SET id='other'").unwrap();
    guarded(&conn, "returned.jpg", false);
    conn.execute_batch("UPDATE assets SET id='master'").unwrap();
    guarded(&conn, "returned.jpg", true);
    conn.execute_batch("UPDATE assets SET library='other'")
        .unwrap();
    guarded(&conn, "returned.jpg", false);
    conn.execute_batch("UPDATE assets SET library='lib'")
        .unwrap();
    guarded(&conn, "returned.jpg", true);
}

#[test]
fn legacy_path_cache_metadata_and_prepared_path_transitions() {
    let conn = database();
    mappings(&conn, "master");
    guarded(&conn, "metadata.jpg", false);
    conn.execute_batch(
        "INSERT INTO asset_metadata_paths(library,id,version_size,local_path,provider_checksum)
        VALUES ('lib','master','original','metadata.jpg','hash')",
    )
    .unwrap();
    guarded(&conn, "metadata.jpg", true);
    conn.execute_batch("UPDATE asset_metadata_paths SET local_path='moved.jpg'")
        .unwrap();
    guarded(&conn, "metadata.jpg", false);
    guarded(&conn, "moved.jpg", true);
    conn.execute_batch("UPDATE asset_metadata_paths SET id='other'")
        .unwrap();
    guarded(&conn, "moved.jpg", false);
    conn.execute_batch("UPDATE asset_metadata_paths SET id='master'")
        .unwrap();
    guarded(&conn, "moved.jpg", true);
    conn.execute_batch("UPDATE asset_metadata_paths SET library='other'")
        .unwrap();
    guarded(&conn, "moved.jpg", false);
    conn.execute_batch("UPDATE asset_metadata_paths SET library='lib'")
        .unwrap();
    guarded(&conn, "moved.jpg", true);
    conn.execute_batch("DELETE FROM asset_metadata_paths")
        .unwrap();
    guarded(&conn, "moved.jpg", false);
    let protected_key = key(Path::new("prepared.jpg.xmp")).unwrap();
    conn.execute(
        "INSERT INTO unattributed_legacy_paths VALUES ('lib','master',?1,'prepared.jpg.xmp')",
        [&protected_key],
    )
    .unwrap();
    guarded(&conn, "prepared.jpg", true);
    guarded(&conn, "prepared.jpg.xmp", true);
    // Migration normally forbids these mutations; exercise cache defenses if a repair changes them.
    conn.execute_batch(
        "DROP TRIGGER unattributed_path_immutable; DROP TRIGGER unattributed_path_retained",
    )
    .unwrap();
    guarded(&conn, "prepared.jpg", true);
    conn.execute(
        "UPDATE unattributed_legacy_paths SET local_path='renamed.jpg',path_key=?1",
        [key(Path::new("renamed.jpg")).unwrap()],
    )
    .unwrap();
    guarded(&conn, "prepared.jpg", false);
    guarded(&conn, "renamed.jpg", true);
    conn.execute_batch("DELETE FROM unattributed_legacy_paths")
        .unwrap();
    guarded(&conn, "renamed.jpg", false);
}

#[test]
fn legacy_path_cache_ordinary_interleaved_writes_keep_warm_and_empty_caches() {
    let conn = database();
    asset(&conn, "master", "protected.jpg");
    mappings(&conn, "master");
    asset(&conn, "single", "single.jpg");
    asset(&conn, "first-master", "first-master.jpg");
    conn.execute_batch(
        "INSERT INTO asset_master_mappings VALUES ('lib','single-child','single',0)",
    )
    .unwrap();
    guarded(&conn, "protected.jpg", true);
    forbid_rebuild(&conn);
    for _ in 0..3 {
        conn.execute_batch("UPDATE assets SET status='downloaded',last_seen_at=last_seen_at+1;
            UPDATE asset_master_mappings SET updated_at=updated_at+1;
            UPDATE assets SET local_path=local_path WHERE id='master';
            UPDATE assets SET local_path='single-moved.jpg' WHERE id='single';
            INSERT OR REPLACE INTO asset_master_mappings VALUES ('lib','first-child','first-master',0)").unwrap();
        guarded(&conn, "protected.jpg", true);
        guarded(&conn, "single-moved.jpg", false);
    }
    asset(&conn, "ordinary", "ordinary.jpg");
    conn.execute_batch(
        "UPDATE assets SET local_path='ordinary-moved.jpg' WHERE id='ordinary';
        DELETE FROM assets WHERE id='ordinary';
        INSERT INTO asset_metadata_paths(library,id,version_size,local_path,provider_checksum)
            VALUES ('lib','master-a','original','child.jpg','hash'),('lib','single','original','single-metadata.jpg','hash');
        UPDATE asset_metadata_paths SET local_path='child-moved.jpg';
        DELETE FROM asset_metadata_paths",
    )
    .unwrap();
    guarded(&conn, "protected.jpg", true);

    let empty = database();
    guarded(&empty, "ordinary.jpg", false);
    // A valid empty index has a validity row, independent of its path count.
    assert_eq!(
        empty
            .query_row::<i64, _, _>(
                "SELECT COUNT(*) FROM temp.legacy_path_cache_valid",
                [],
                |r| r.get(0)
            )
            .unwrap(),
        1
    );
    empty
        .execute_batch(
            "CREATE TEMP TRIGGER reject_empty_rebuild BEFORE INSERT ON legacy_path_cache_valid
        BEGIN SELECT RAISE(ABORT,'unexpected empty rebuild'); END;
        INSERT INTO asset_master_mappings VALUES ('lib','first','master',0)",
        )
        .unwrap();
    guarded(&empty, "ordinary.jpg", false);
}

#[test]
fn legacy_path_cache_rolls_back_rebuilt_and_first_created_cache() {
    let conn = database();
    asset(&conn, "master", "original.jpg");
    mappings(&conn, "master");
    guarded(&conn, "original.jpg", true);
    conn.execute_batch("BEGIN; UPDATE assets SET local_path='transaction.jpg'")
        .unwrap();
    guarded(&conn, "original.jpg", false);
    guarded(&conn, "transaction.jpg", true);
    conn.execute_batch("ROLLBACK").unwrap();
    forbid_rebuild(&conn);
    guarded(&conn, "original.jpg", true);
    guarded(&conn, "transaction.jpg", false);

    let fresh = database();
    fresh.execute_batch("BEGIN").unwrap();
    guarded(&fresh, "ordinary.jpg", false);
    fresh.execute_batch("ROLLBACK").unwrap();
    asset(&fresh, "master", "original.jpg");
    mappings(&fresh, "master");
    guarded(&fresh, "original.jpg", true);
}

#[test]
fn legacy_path_cache_external_commits_schema_drift_and_query_only() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    let conn = Connection::open(&path).unwrap();
    crate::state::schema::migrate(&conn).unwrap();
    asset(&conn, "master", "original.jpg");
    mappings(&conn, "master");
    guarded(&conn, "original.jpg", true);
    let other = Connection::open(&path).unwrap();
    other
        .execute_batch("UPDATE assets SET local_path='external.jpg'")
        .unwrap();
    guarded(&conn, "original.jpg", false);
    guarded(&conn, "external.jpg", true);
    conn.execute_batch("CREATE TABLE unrelated(value TEXT)")
        .unwrap();
    guarded(&conn, "external.jpg", true);
    let changes = conn.total_changes();
    conn.pragma_update(None, "query_only", true).unwrap();
    guarded(&conn, "external.jpg", true);
    assert!(
        protected_paths(&conn)
            .unwrap()
            .contains(&"external.jpg".into())
    );
    assert_eq!(conn.total_changes(), changes);
    conn.pragma_update(None, "query_only", false).unwrap();
    conn.execute_batch("DROP TABLE asset_metadata_paths")
        .unwrap();
    assert!(path_is_protected(&conn, Path::new("external.jpg")).is_err());
}

#[test]
#[ignore = "manual catalogue-size performance measurement"]
fn legacy_path_cache_benchmark() {
    for count in [100_000, 300_000] {
        for catalogue in [false, true] {
            let conn = database();
            conn.execute_batch(&format!(
                "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM n WHERE i<{count})
                INSERT INTO asset_master_mappings SELECT 'lib','child-'||i,'master-'||i,0 FROM n;"
            ))
            .unwrap();
            if catalogue {
                conn.execute_batch(&format!(
                    "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM n WHERE i<{count})
                    INSERT INTO assets(library,id,version_size,checksum,filename,created_at,size_bytes,
                        media_type,last_seen_at,local_path)
                    SELECT 'lib','child-'||i,'original','hash','file',0,1,'photo',0,'ordinary/'||i||'.jpg' FROM n;"
                )).unwrap();
                conn.execute_batch(
                    "INSERT INTO asset_metadata_paths(library,id,version_size,local_path,provider_checksum)
                        SELECT library,id,version_size,local_path,checksum FROM assets"
                ).unwrap();
            }
            asset(&conn, "master-1", "single-master.jpg");
            asset(&conn, "protected", "protected.jpg");
            mappings(&conn, "protected");
            let start = Instant::now();
            let mut old = conn.prepare(&format!(
                "WITH ambiguous(library,id) AS ({})
                SELECT a.local_path FROM assets a JOIN ambiguous m USING(library,id) WHERE a.local_path IS NOT NULL
                UNION SELECT p.local_path FROM asset_metadata_paths p JOIN ambiguous m USING(library,id)",
                super::super::AMBIGUOUS_MASTERS
            )).unwrap();
            let old_paths = old
                .query_map([], |r| r.get::<_, String>(0))
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap();
            assert_eq!(old_paths, ["protected.jpg"]);
            let old_census = start.elapsed();
            let start = Instant::now();
            assert!(path_is_protected(&conn, Path::new("protected.jpg")).unwrap());
            let cold = start.elapsed();
            forbid_rebuild(&conn);
            let start = Instant::now();
            for _ in 0..1_000 {
                assert!(path_is_protected(&conn, Path::new("protected.jpg")).unwrap());
            }
            let warm = start.elapsed();
            let start = Instant::now();
            for i in 0..1_000 {
                conn.execute("UPDATE asset_master_mappings SET updated_at=updated_at+1 WHERE library='lib' AND asset_record_name=?1", [format!("child-{}", i + 1)]).unwrap();
                conn.execute(
                    "UPDATE assets SET local_path=?1 WHERE library='lib' AND id='master-1'",
                    [format!("single-master-{i}.jpg")],
                )
                .unwrap();
                conn.execute(
                    "INSERT INTO asset_master_mappings VALUES ('lib',?1,?2,0)",
                    params![format!("new-child-{i}"), format!("new-master-{i}")],
                )
                .unwrap();
                if catalogue {
                    conn.execute("UPDATE assets SET last_seen_at=last_seen_at+1,local_path='ordinary-updated.jpg' WHERE library='lib' AND id=?1", [format!("child-{}", i + 1)]).unwrap();
                    conn.execute("UPDATE asset_metadata_paths SET local_path='metadata-updated.jpg' WHERE library='lib' AND id=?1", [format!("child-{}", i + 1)]).unwrap();
                }
                assert!(path_is_protected(&conn, Path::new("protected.jpg")).unwrap());
            }
            eprintln!(
                "legacy cache rows={count} catalogue={catalogue}: old-census={old_census:?} cold={cold:?} warm/1000={warm:?} interleaved/1000={:?}",
                start.elapsed()
            );
        }
    }
}
