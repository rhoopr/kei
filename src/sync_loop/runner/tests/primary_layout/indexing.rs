//! Managed indexing recovery uses real readiness, reopened state and file owners.
use super::{
    CycleOptions, EditedNaming, IndexingOptions, ORIGINAL, assert_bytes, cycle, cycle_options,
    records, server, try_cycle_options,
};
use crate::sync_cycle::{
    indexing_materialization_key, indexing_reconciliation_key, indexing_replay_key,
};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex, atomic::AtomicUsize};
use tokio_util::sync::CancellationToken;

const ZONE: &str = "PrimarySync";
const CURSOR: &str = "sync_token:PrimarySync";

fn readiness(state: &'static str) -> IndexingOptions {
    IndexingOptions {
        readiness: Arc::new(Mutex::new(state)),
        regress: false,
        cancel: CancellationToken::new(),
        interrupt: false,
        stale_plan: false,
        recent: None,
        retry_only: false,
        token: Some("query-token"),
        queries: Arc::new(AtomicUsize::new(0)),
        cursors: Arc::new(Mutex::new(Vec::new())),
    }
}
fn options(indexing: &IndexingOptions) -> CycleOptions {
    CycleOptions {
        indexing: Some(indexing.clone()),
        ..CycleOptions::default()
    }
}
fn metadata(root: &Path, key: &str) -> Option<String> {
    use rusqlite::OptionalExtension;
    rusqlite::Connection::open(root.join("state.db"))
        .unwrap()
        .query_row("SELECT value FROM metadata WHERE key=?1", [key], |row| {
            row.get(0)
        })
        .optional()
        .unwrap()
}
fn media(root: &Path) -> BTreeMap<std::path::PathBuf, Vec<u8>> {
    fn visit(root: &Path, at: &Path, result: &mut BTreeMap<std::path::PathBuf, Vec<u8>>) {
        for item in std::fs::read_dir(at).unwrap() {
            let path = item.unwrap().path();
            if path.is_dir() {
                visit(root, &path, result);
            } else {
                result.insert(
                    path.strip_prefix(root).unwrap().to_owned(),
                    std::fs::read(&path).unwrap(),
                );
            }
        }
    }
    let mut result = BTreeMap::new();
    visit(&root.join("media"), &root.join("media"), &mut result);
    result
}
fn managed_state(root: &Path) -> Vec<String> {
    let conn = rusqlite::Connection::open(root.join("state.db")).unwrap();
    [
        "SELECT hex(binding) FROM primary_layout_bindings ORDER BY family",
        "SELECT hex(header)||phase||COALESCE(conflict,'') FROM primary_layout_operations ORDER BY operation",
        "SELECT hex(evidence) FROM primary_layout_members ORDER BY operation,member",
        "SELECT hex(evidence) FROM primary_layout_preserved ORDER BY operation,member",
        "SELECT id||version_size||status||COALESCE(local_path,'') FROM assets ORDER BY id,version_size",
    ].into_iter().flat_map(|sql| conn.prepare(sql).unwrap().query_map([], |row| row.get::<_,String>(0)).unwrap().collect::<Result<Vec<_>,_>>().unwrap()).collect()
}
fn assert_hold(root: &Path, cursor: &str, full: bool, materialization: bool) {
    assert_eq!(metadata(root, CURSOR).as_deref(), Some(cursor));
    assert_eq!(
        metadata(root, &indexing_reconciliation_key(ZONE)).is_some(),
        full
    );
    assert_eq!(
        metadata(root, &indexing_materialization_key(ZONE)).is_some(),
        materialization
    );
}
fn current_with_new(server: &wiremock::MockServer, endpoint: &str) -> Vec<Value> {
    let mut all = records(server, None);
    let mut new = records(server, None);
    for record in &mut new {
        record["recordName"] = json!(record["recordName"].as_str().unwrap().replace("501", "502"));
        if let Some(reference) = record["fields"].get_mut("masterRef") {
            reference["value"]["recordName"] = json!("502");
        }
    }
    new[0]["fields"]["filenameEnc"]["value"] = json!("NEW_0502.JPG");
    new[0]["fields"]["resOriginalRes"]["value"] = json!({
        "downloadURL": format!("{}/{endpoint}", server.uri()),
        "size": super::EDIT_TWO.len(),
        "fileChecksum": super::provider_hash(super::EDIT_TWO)
    });
    all.extend(new);
    all
}

#[tokio::test]
async fn indexing_managed_reopen_reconciles_omissions_with_quiet_feed() {
    for naming in [EditedNaming::Primary, EditedNaming::Suffix] {
        let server = server().await;
        let root = tempfile::tempdir().unwrap();
        let initial_records = records(&server, None);
        let (initial, _) = cycle(
            root.path(),
            initial_records.clone(),
            false,
            EditedNaming::Primary,
        )
        .await;
        assert!(initial.can_advance_database_checkpoint());
        let cursor = metadata(root.path(), CURSOR).unwrap();
        let before_media = media(root.path());
        let before_state = managed_state(root.path());
        let gets = server.received_requests().await.unwrap().len();
        for _ in 0..2 {
            let running = readiness("RUNNING");
            let (result, _) = cycle_options(
                root.path(),
                initial_records.clone(),
                true,
                naming,
                options(&running),
            )
            .await;
            assert_eq!(result.failed_count, 0, "{:?}", result.stats);
            assert_eq!(result.stats.failed, 0);
            assert_eq!(result.stats.skipped.uncertain, 0);
            assert!(!result.can_advance_database_checkpoint());
            assert_hold(root.path(), &cursor, true, false);
            assert_eq!(media(root.path()), before_media);
            assert_eq!(managed_state(root.path()), before_state);
            assert_eq!(server.received_requests().await.unwrap().len(), gets);
        }
        let mut retry = readiness("FINISHED");
        retry.retry_only = true;
        let (result, _) = cycle_options(
            root.path(),
            initial_records.clone(),
            true,
            naming,
            options(&retry),
        )
        .await;
        assert_eq!(result.failed_count, 0, "{:?}", result.stats);
        assert_eq!(result.stats.failed, 0);
        assert_eq!(result.stats.skipped.uncertain, 0);
        assert!(!result.can_advance_database_checkpoint());
        assert_hold(root.path(), &cursor, true, false);
        assert_eq!(media(root.path()), before_media);
        assert_eq!(managed_state(root.path()), before_state);
        let mut all = initial_records.clone();
        let mut omitted = initial_records.clone();
        for record in &mut omitted {
            record["recordName"] =
                json!(record["recordName"].as_str().unwrap().replace("501", "502"));
            if let Some(reference) = record["fields"].get_mut("masterRef") {
                reference["value"]["recordName"] = json!("502");
            }
        }
        omitted[0]["fields"]["filenameEnc"]["value"] = json!("IMG_0502.JPG");
        omitted[0]["fields"]["resOriginalRes"]["value"] = json!({"downloadURL": format!("{}/edit-two", server.uri()), "size": super::EDIT_TWO.len(), "fileChecksum": super::provider_hash(super::EDIT_TWO)});
        all.extend(omitted);
        let finished = readiness("FINISHED");
        let (recovered, _) =
            cycle_options(root.path(), all.clone(), true, naming, options(&finished)).await;
        assert_eq!(
            recovered.failed_count, 0,
            "{naming:?}: {:?}",
            recovered.stats
        );
        assert!(!recovered.can_advance_database_checkpoint());
        assert_hold(root.path(), &cursor, false, false);
        assert!(metadata(root.path(), &indexing_replay_key(ZONE)).is_some());
        assert_bytes(&root.path().join("media/IMG_0501.JPG"), ORIGINAL);
        assert_bytes(&root.path().join("media/IMG_0502.JPG"), super::EDIT_TWO);
        let replay = readiness("FINISHED");
        let (recovered, _) =
            cycle_options(root.path(), all.clone(), true, naming, options(&replay)).await;
        assert_eq!(
            recovered.failed_count, 0,
            "{naming:?}: {:?}",
            recovered.stats
        );
        assert!(
            recovered.can_advance_database_checkpoint(),
            "{naming:?}: {:?}",
            recovered.stats
        );
        assert_eq!(*replay.cursors.lock().unwrap(), [cursor]);
        assert_hold(root.path(), "cycle-token", false, false);
        assert!(metadata(root.path(), &indexing_replay_key(ZONE)).is_none());
        assert_bytes(&root.path().join("media/IMG_0501.JPG"), ORIGINAL);
        assert_bytes(&root.path().join("media/IMG_0502.JPG"), super::EDIT_TWO);
        assert!(!root.path().join("media/IMG_0501_original.JPG").exists());
        let gets = server.received_requests().await.unwrap().len();
        for _ in 0..2 {
            let steady = readiness("FINISHED");
            let (result, _) =
                cycle_options(root.path(), all.clone(), true, naming, options(&steady)).await;
            assert!(
                result.can_advance_database_checkpoint(),
                "{:?}",
                result.stats
            );
            assert_eq!(result.stats.downloaded, 0);
            assert_eq!(steady.queries.load(std::sync::atomic::Ordering::SeqCst), 0);
            assert_eq!(server.received_requests().await.unwrap().len(), gets);
        }
    }
}

#[tokio::test]
async fn indexing_managed_inventory_faults_never_release_hold() {
    for fault in [
        "regression",
        "cancel",
        "bound",
        "stale",
        "identity",
        "token",
        "write",
    ] {
        let server = server().await;
        let root = tempfile::tempdir().unwrap();
        let original = records(&server, None);
        let (initial, _) = cycle(root.path(), original.clone(), false, EditedNaming::Primary).await;
        assert!(initial.can_advance_database_checkpoint());
        let cursor = metadata(root.path(), CURSOR).unwrap();
        let (running, _) = cycle_options(
            root.path(),
            original,
            false,
            EditedNaming::Primary,
            options(&readiness("RUNNING")),
        )
        .await;
        assert!(!running.can_advance_database_checkpoint());
        assert_eq!(running.stats.skipped.uncertain, 0);
        assert_hold(root.path(), &cursor, true, false);
        let mut current = current_with_new(&server, "edit-two");
        let mut finished = readiness("FINISHED");
        match fault {
            "regression" => finished.regress = true,
            "cancel" => finished.interrupt = true,
            "bound" => finished.recent = Some(1),
            "stale" => finished.stale_plan = true,
            "identity" => current[3]["recordName"] = json!(""),
            "token" => finished.token = None,
            "write" => rusqlite::Connection::open(root.path().join("state.db")).unwrap().execute_batch(
                "CREATE TRIGGER fail_indexing_qualification BEFORE INSERT ON metadata WHEN NEW.key LIKE 'indexing_replay_required:%' BEGIN SELECT RAISE(FAIL,'injected qualification failure'); END"
            ).unwrap(),
            _ => panic!("unknown fixture fault: {fault}"),
        }
        let (held, _) = cycle_options(
            root.path(),
            current,
            false,
            EditedNaming::Primary,
            options(&finished),
        )
        .await;
        assert!(!held.can_advance_database_checkpoint(), "{fault}: {held:?}");
        assert_hold(root.path(), &cursor, true, false);
        assert!(
            metadata(root.path(), &indexing_replay_key(ZONE)).is_none(),
            "{fault}"
        );
        assert_bytes(&root.path().join("media/IMG_0501.JPG"), ORIGINAL);
        let new_path = root.path().join("media/NEW_0502.JPG");
        if new_path.exists() {
            assert_bytes(&new_path, super::EDIT_TWO);
        }
        if fault == "write" {
            rusqlite::Connection::open(root.path().join("state.db"))
                .unwrap()
                .execute_batch("DROP TRIGGER fail_indexing_qualification")
                .unwrap();
        }
        let good = current_with_new(&server, "edit-two");
        let (qualified, _) = cycle_options(
            root.path(),
            good.clone(),
            false,
            EditedNaming::Primary,
            options(&readiness("FINISHED")),
        )
        .await;
        assert_eq!(qualified.failed_count, 0, "{fault}: {qualified:?}");
        assert!(!qualified.can_advance_database_checkpoint());
        assert_hold(root.path(), &cursor, false, false);
        assert!(metadata(root.path(), &indexing_replay_key(ZONE)).is_some());
        assert_bytes(&new_path, super::EDIT_TWO);
        let replay = readiness("FINISHED");
        let (recovered, _) = cycle_options(
            root.path(),
            good.clone(),
            false,
            EditedNaming::Primary,
            options(&replay),
        )
        .await;
        assert!(
            recovered.can_advance_database_checkpoint(),
            "{fault}: {recovered:?}"
        );
        assert_eq!(*replay.cursors.lock().unwrap(), [cursor]);
        assert_hold(root.path(), "cycle-token", false, false);
        assert!(metadata(root.path(), &indexing_replay_key(ZONE)).is_none());
        let before = media(root.path());
        let (steady, _) = cycle_options(
            root.path(),
            good,
            true,
            EditedNaming::Primary,
            options(&readiness("FINISHED")),
        )
        .await;
        assert!(
            steady.can_advance_database_checkpoint(),
            "{fault}: {steady:?}"
        );
        assert_eq!(steady.stats.downloaded, 0);
        assert_eq!(media(root.path()), before);
    }
}

#[tokio::test]
async fn indexing_managed_reconciliation_faults_keep_cursor_until_recovered() {
    for fault in ["transfer", "commit", "cancel", "bound", "regression"] {
        let server = server().await;
        let root = tempfile::tempdir().unwrap();
        let original = records(&server, None);
        let (initial, _) = cycle(root.path(), original.clone(), false, EditedNaming::Primary).await;
        assert!(initial.can_advance_database_checkpoint());
        let cursor = metadata(root.path(), CURSOR).unwrap();
        let (running, _) = cycle_options(
            root.path(),
            original.clone(),
            false,
            EditedNaming::Primary,
            options(&readiness("RUNNING")),
        )
        .await;
        assert!(!running.can_advance_database_checkpoint());
        assert_hold(root.path(), &cursor, true, false);
        let mut finished = readiness("FINISHED");
        let current = current_with_new(
            &server,
            if fault == "transfer" {
                "unavailable"
            } else {
                "edit-two"
            },
        );
        match fault {
            "commit" => rusqlite::Connection::open(root.path().join("state.db")).unwrap().execute_batch(
                "CREATE TRIGGER fail_recovery_checkpoint BEFORE INSERT ON metadata WHEN NEW.key LIKE 'indexing_replay_required:%' BEGIN SELECT RAISE(FAIL,'injected qualification failure'); END"
            ).unwrap(),
            "cancel" => finished.interrupt = true,
            "bound" => finished.recent = Some(1),
            "regression" => finished.regress = true,
            _ => {}
        }
        let (held, _) = cycle_options(
            root.path(),
            current,
            false,
            EditedNaming::Primary,
            options(&finished),
        )
        .await;
        assert!(!held.can_advance_database_checkpoint(), "{fault}: {held:?}");
        assert_hold(root.path(), &cursor, true, false);
        assert!(metadata(root.path(), &indexing_replay_key(ZONE)).is_none());
        assert_bytes(&root.path().join("media/IMG_0501.JPG"), ORIGINAL);
        if fault == "transfer" {
            let conn = rusqlite::Connection::open(root.path().join("state.db")).unwrap();
            let (status, error): (String, Option<String>) = conn.query_row(
                "SELECT status,last_error FROM assets WHERE library='PrimarySync' AND id='asset-502' AND version_size='original'",
                [], |row| Ok((row.get(0)?, row.get(1)?))
            ).unwrap();
            assert_eq!(status, "failed");
            assert!(error.is_some());
        }
        if fault == "commit" {
            assert!(held.stats.state_write_failures > 0, "{held:?}");
            rusqlite::Connection::open(root.path().join("state.db"))
                .unwrap()
                .execute_batch("DROP TRIGGER fail_recovery_checkpoint")
                .unwrap();
        }
        let good = current_with_new(&server, "edit-two");
        let (qualified, _) = cycle_options(
            root.path(),
            good.clone(),
            false,
            EditedNaming::Primary,
            options(&readiness("FINISHED")),
        )
        .await;
        assert_eq!(qualified.failed_count, 0, "{fault}: {qualified:?}");
        assert!(!qualified.can_advance_database_checkpoint());
        assert_hold(root.path(), &cursor, false, false);
        assert!(metadata(root.path(), &indexing_replay_key(ZONE)).is_some());
        assert_bytes(&root.path().join("media/NEW_0502.JPG"), super::EDIT_TWO);
        let replay = readiness("FINISHED");
        let (recovered, _) = cycle_options(
            root.path(),
            good.clone(),
            false,
            EditedNaming::Primary,
            options(&replay),
        )
        .await;
        assert!(
            recovered.can_advance_database_checkpoint(),
            "{fault}: {recovered:?}"
        );
        assert_eq!(*replay.cursors.lock().unwrap(), [cursor]);
        assert_hold(root.path(), "cycle-token", false, false);
        assert!(metadata(root.path(), &indexing_replay_key(ZONE)).is_none());
        let before = media(root.path());
        let (steady, _) = cycle_options(
            root.path(),
            good,
            true,
            EditedNaming::Primary,
            options(&readiness("FINISHED")),
        )
        .await;
        assert!(
            steady.can_advance_database_checkpoint(),
            "{fault}: {steady:?}"
        );
        assert_eq!(steady.stats.downloaded, 0);
        assert_eq!(media(root.path()), before);
    }
}

#[tokio::test]
async fn indexing_managed_legacy_materialization_debt_survives_failure_and_reopen() {
    let server = server().await;
    let root = tempfile::tempdir().unwrap();
    let original = records(&server, None);
    let (initial, _) = cycle(root.path(), original, false, EditedNaming::Primary).await;
    assert!(initial.can_advance_database_checkpoint());
    let cursor = metadata(root.path(), CURSOR).unwrap();
    let conn = rusqlite::Connection::open(root.path().join("state.db")).unwrap();
    conn.execute(
        "INSERT INTO metadata(key,value) VALUES(?1,'legacy-debt')",
        [&indexing_materialization_key(ZONE)],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO metadata(key,value) VALUES(?1,'legacy-replay')",
        [&indexing_replay_key(ZONE)],
    )
    .unwrap();
    drop(conn);
    let (held, _) = cycle_options(
        root.path(),
        current_with_new(&server, "unavailable"),
        false,
        EditedNaming::Primary,
        options(&readiness("FINISHED")),
    )
    .await;
    assert!(!held.can_advance_database_checkpoint(), "{held:?}");
    assert_hold(root.path(), &cursor, false, true);
    assert_eq!(
        metadata(root.path(), &indexing_materialization_key(ZONE)).as_deref(),
        Some("legacy-debt")
    );
    assert_eq!(
        metadata(root.path(), &indexing_replay_key(ZONE)).as_deref(),
        Some("legacy-replay")
    );
    let conn = rusqlite::Connection::open(root.path().join("state.db")).unwrap();
    let (status, error): (String, Option<String>) = conn.query_row(
        "SELECT status,last_error FROM assets WHERE library='PrimarySync' AND id='asset-502' AND version_size='original'",
        [], |row| Ok((row.get(0)?, row.get(1)?))
    ).unwrap();
    assert_eq!(status, "failed");
    assert!(error.is_some());
    drop(conn);
    assert_bytes(&root.path().join("media/IMG_0501.JPG"), ORIGINAL);
    let current = current_with_new(&server, "edit-two");
    let conn = rusqlite::Connection::open(root.path().join("state.db")).unwrap();
    conn.execute_batch(
        "CREATE TRIGGER fail_cursor_promotion BEFORE UPDATE OF value ON metadata WHEN NEW.key='sync_token:PrimarySync' BEGIN SELECT RAISE(FAIL,'injected cursor promotion failure'); END"
    ).unwrap();
    drop(conn);
    let failed_bridge = readiness("FINISHED");
    let (failed, _) = cycle_options(
        root.path(),
        current.clone(),
        false,
        EditedNaming::Primary,
        options(&failed_bridge),
    )
    .await;
    assert!(!failed.can_advance_database_checkpoint(), "{failed:?}");
    assert!(failed.stats.state_write_failures > 0, "{failed:?}");
    assert_eq!(
        failed_bridge.cursors.lock().unwrap().as_slice(),
        std::slice::from_ref(&cursor)
    );
    assert_bytes(&root.path().join("media/IMG_0501.JPG"), ORIGINAL);
    assert_bytes(&root.path().join("media/NEW_0502.JPG"), super::EDIT_TWO);
    // Each metadata call reopens the database after the failed transaction.
    assert_hold(root.path(), &cursor, false, true);
    assert_eq!(
        metadata(root.path(), &indexing_materialization_key(ZONE)).as_deref(),
        Some("legacy-debt")
    );
    assert_eq!(
        metadata(root.path(), &indexing_replay_key(ZONE)).as_deref(),
        Some("legacy-replay")
    );
    let conn = rusqlite::Connection::open(root.path().join("state.db")).unwrap();
    let status: String = conn.query_row(
        "SELECT status FROM assets WHERE library='PrimarySync' AND id='asset-502' AND version_size='original'",
        [], |row| row.get(0),
    ).unwrap();
    assert_eq!(status, "downloaded");
    conn.execute_batch("DROP TRIGGER fail_cursor_promotion")
        .unwrap();
    drop(conn);
    let replay = readiness("FINISHED");
    let (recovered, _) = cycle_options(
        root.path(),
        current.clone(),
        false,
        EditedNaming::Primary,
        options(&replay),
    )
    .await;
    assert!(recovered.can_advance_database_checkpoint(), "{recovered:?}");
    assert_eq!(*replay.cursors.lock().unwrap(), [cursor]);
    assert_hold(root.path(), "cycle-token", false, false);
    assert!(metadata(root.path(), &indexing_replay_key(ZONE)).is_none());
    assert_bytes(&root.path().join("media/IMG_0501.JPG"), ORIGINAL);
    assert_bytes(&root.path().join("media/NEW_0502.JPG"), super::EDIT_TWO);
    let before = media(root.path());
    let (steady, _) = cycle_options(
        root.path(),
        current,
        true,
        EditedNaming::Primary,
        options(&readiness("FINISHED")),
    )
    .await;
    assert!(steady.can_advance_database_checkpoint(), "{steady:?}");
    assert_eq!(steady.stats.downloaded, 0);
    assert_eq!(media(root.path()), before);
}

#[tokio::test]
async fn indexing_managed_pending_journal_is_held_even_under_suffix_then_recovers() {
    let server = server().await;
    let root = tempfile::tempdir().unwrap();
    let current = records(&server, None);
    let (initial, _) = cycle(root.path(), current.clone(), false, EditedNaming::Primary).await;
    assert!(initial.can_advance_database_checkpoint());
    let cursor = metadata(root.path(), CURSOR).unwrap();
    let db = Arc::new(
        crate::state::SqliteStateDb::open(&root.path().join("state.db"))
            .await
            .unwrap(),
    );
    let operation = crate::download::prepared_layout_with_refresh(
        &db,
        &root.path().join("media"),
        "recorded-recovery.JPG",
        ZONE,
        true,
    )
    .await;
    let destination = operation.members[0].destination.as_ref().unwrap().to_path();
    db.acquire_lock("inject recorded layout commit failure").unwrap().execute_batch(
        "CREATE TRIGGER fail_layout_commit BEFORE INSERT ON primary_layout_bindings BEGIN SELECT RAISE(FAIL,'injected journal commit failure'); END"
    ).unwrap();
    drop(db);
    let running = readiness("RUNNING");
    let error = try_cycle_options(
        root.path(),
        current.clone(),
        false,
        EditedNaming::Suffix,
        options(&running),
    )
    .await
    .unwrap_err();
    assert!(format!("{error:#}").contains("injected journal commit failure"));
    assert_eq!(
        metadata(root.path(), CURSOR).as_deref(),
        Some(cursor.as_str())
    );
    assert_bytes(&destination, b"prepared current rendition");
    assert_bytes(&root.path().join("media/IMG_0501.JPG"), ORIGINAL);
    let db = crate::state::SqliteStateDb::open(&root.path().join("state.db"))
        .await
        .unwrap();
    let pending = db.primary_layout_operations(ZONE.into()).await.unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].phase, "publishing");
    assert!(pending[0].refresh_metadata);
    assert!(pending[0].members[0].installed.is_some());
    db.acquire_lock("clear recorded layout commit failure")
        .unwrap()
        .execute_batch("DROP TRIGGER fail_layout_commit")
        .unwrap();
    drop(db);
    let (held, _) = cycle_options(
        root.path(),
        current.clone(),
        false,
        EditedNaming::Suffix,
        options(&readiness("RUNNING")),
    )
    .await;
    assert!(!held.can_advance_database_checkpoint());
    assert_hold(root.path(), &cursor, true, false);
    assert_bytes(&destination, b"prepared current rendition");
    let db = crate::state::SqliteStateDb::open(&root.path().join("state.db"))
        .await
        .unwrap();
    assert!(
        db.primary_layout_operations(ZONE.into())
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        db.primary_layout_binding(operation.family.clone())
            .await
            .unwrap()
            .is_some()
    );
    drop(db);
    let (inventory, _) = cycle_options(
        root.path(),
        current.clone(),
        false,
        EditedNaming::Suffix,
        options(&readiness("FINISHED")),
    )
    .await;
    assert!(
        !inventory.can_advance_database_checkpoint(),
        "{inventory:?}"
    );
    assert_hold(root.path(), &cursor, false, false);
    assert!(metadata(root.path(), &indexing_replay_key(ZONE)).is_some());
    let replay = readiness("FINISHED");
    let (recovered, _) = cycle_options(
        root.path(),
        current.clone(),
        false,
        EditedNaming::Suffix,
        options(&replay),
    )
    .await;
    assert!(recovered.can_advance_database_checkpoint(), "{recovered:?}");
    assert_eq!(*replay.cursors.lock().unwrap(), [cursor]);
    assert_hold(root.path(), "cycle-token", false, false);
    assert!(metadata(root.path(), &indexing_replay_key(ZONE)).is_none());
    assert_bytes(&destination, b"prepared current rendition");
    assert_bytes(&root.path().join("media/IMG_0501.JPG"), ORIGINAL);
    let before = media(root.path());
    let (steady, _) = cycle_options(
        root.path(),
        current,
        true,
        EditedNaming::Suffix,
        options(&readiness("FINISHED")),
    )
    .await;
    assert!(steady.can_advance_database_checkpoint(), "{steady:?}");
    assert_eq!(steady.stats.downloaded, 0);
    assert_eq!(media(root.path()), before);
}
#[tokio::test]
async fn indexing_managed_local_modification_and_foreign_collision_remain_pending() {
    let server = server().await;
    let root = tempfile::tempdir().unwrap();
    let original = records(&server, None);
    let (initial, _) = cycle(root.path(), original.clone(), false, EditedNaming::Primary).await;
    assert!(initial.can_advance_database_checkpoint());
    let cursor = metadata(root.path(), CURSOR).unwrap();
    let (running, _) = cycle_options(
        root.path(),
        original,
        false,
        EditedNaming::Primary,
        options(&readiness("RUNNING")),
    )
    .await;
    assert!(!running.can_advance_database_checkpoint());
    assert_hold(root.path(), &cursor, true, false);
    let target = root.path().join("media");
    let local = b"independent local modification";
    let foreign = b"independent foreign collision";
    std::fs::write(target.join("IMG_0501.JPG"), local).unwrap();
    std::fs::write(target.join("NEW_0502.JPG"), foreign).unwrap();
    let current = current_with_new(&server, "edit-two");
    let (held, _) = cycle_options(
        root.path(),
        current.clone(),
        false,
        EditedNaming::Primary,
        options(&readiness("FINISHED")),
    )
    .await;
    assert!(!held.can_advance_database_checkpoint(), "{held:?}");
    assert_eq!(held.stats.skipped.uncertain, 2);
    assert_hold(root.path(), &cursor, true, false);
    assert_bytes(&target.join("IMG_0501.JPG"), local);
    assert_bytes(&target.join("NEW_0502.JPG"), foreign);
    let db = crate::state::SqliteStateDb::open_read_only(&root.path().join("state.db"))
        .await
        .unwrap();
    let pending = db.get_pending().await.unwrap();
    assert!(pending.iter().any(|p| {
        p.id.as_ref() == "asset-501"
            && p.last_error
                .as_deref()
                .is_some_and(|e| e.starts_with("existing_match_v1:"))
    }));
    assert!(pending.iter().any(|p| {
        p.id.as_ref() == "asset-502"
            && p.last_error
                .as_deref()
                .is_some_and(|e| e.contains("unsupported_provider_proof_without_valid_receipt"))
    }));
    let receipt: (String, String, String) = rusqlite::Connection::open(root.path().join("state.db")).unwrap().query_row(
        "SELECT local_checksum, source_checksum, provider_checksum FROM asset_metadata_paths WHERE library='PrimarySync' AND id='asset-501' AND version_size='original'",
        [], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?))
    ).unwrap();
    assert_eq!(receipt.0, receipt.1);
    assert_eq!(receipt.2, super::provider_hash(ORIGINAL));
    drop(db);
    // The fixture operator restores the owned bytes and removes its own collision.
    std::fs::write(target.join("IMG_0501.JPG"), ORIGINAL).unwrap();
    std::fs::remove_file(target.join("NEW_0502.JPG")).unwrap();
    let (qualified, _) = cycle_options(
        root.path(),
        current.clone(),
        false,
        EditedNaming::Primary,
        options(&readiness("FINISHED")),
    )
    .await;
    assert_eq!(qualified.failed_count, 0, "{qualified:?}");
    assert!(!qualified.can_advance_database_checkpoint());
    assert_hold(root.path(), &cursor, false, false);
    assert!(metadata(root.path(), &indexing_replay_key(ZONE)).is_some());
    assert_bytes(&target.join("IMG_0501.JPG"), ORIGINAL);
    assert_bytes(&target.join("NEW_0502.JPG"), super::EDIT_TWO);
    let replay = readiness("FINISHED");
    let (recovered, _) = cycle_options(
        root.path(),
        current.clone(),
        false,
        EditedNaming::Primary,
        options(&replay),
    )
    .await;
    assert!(recovered.can_advance_database_checkpoint(), "{recovered:?}");
    assert_eq!(*replay.cursors.lock().unwrap(), [cursor]);
    assert_hold(root.path(), "cycle-token", false, false);
    let db = crate::state::SqliteStateDb::open_read_only(&root.path().join("state.db"))
        .await
        .unwrap();
    assert!(db.get_pending().await.unwrap().is_empty());
    drop(db);
    let before = media(root.path());
    let (steady, _) = cycle_options(
        root.path(),
        current,
        true,
        EditedNaming::Primary,
        options(&readiness("FINISHED")),
    )
    .await;
    assert!(steady.can_advance_database_checkpoint(), "{steady:?}");
    assert_eq!(steady.stats.downloaded, 0);
    assert_eq!(media(root.path()), before);
}

#[cfg(feature = "xmp")]
#[tokio::test]
async fn indexing_managed_sidecar_modification_is_preserved_during_progress() {
    let server =
        super::server_with_media(super::VALID_ORIGINAL, super::VALID_EDIT, super::EDIT_ONE).await;
    let root = tempfile::tempdir().unwrap();
    let make_options = |indexing: IndexingOptions| CycleOptions {
        indexing: Some(indexing),
        sidecar: true,
        ..CycleOptions::default()
    };
    let original = super::records_with_original(&server, super::VALID_ORIGINAL, None);
    let (initial, _) = cycle_options(
        root.path(),
        original.clone(),
        false,
        EditedNaming::Primary,
        make_options(readiness("FINISHED")),
    )
    .await;
    assert!(initial.can_advance_database_checkpoint());
    let cursor = metadata(root.path(), CURSOR).unwrap();
    let path = root.path().join("media/IMG_0501.JPG.xmp");
    let local_sidecar = b"independent local sidecar contents";
    std::fs::write(&path, local_sidecar).unwrap();
    let (running, _) = cycle_options(
        root.path(),
        original.clone(),
        false,
        EditedNaming::Primary,
        make_options(readiness("RUNNING")),
    )
    .await;
    assert!(!running.can_advance_database_checkpoint());
    assert_hold(root.path(), &cursor, true, false);
    assert_bytes(&path, local_sidecar);
    assert_bytes(
        &root.path().join("media/IMG_0501.JPG"),
        super::VALID_ORIGINAL,
    );
    let (qualified, _) = cycle_options(
        root.path(),
        original.clone(),
        false,
        EditedNaming::Primary,
        make_options(readiness("FINISHED")),
    )
    .await;
    assert_eq!(qualified.failed_count, 0, "{qualified:?}");
    assert!(!qualified.can_advance_database_checkpoint());
    assert_hold(root.path(), &cursor, false, false);
    assert!(metadata(root.path(), &indexing_replay_key(ZONE)).is_some());
    assert_bytes(&path, local_sidecar);
    let replay = readiness("FINISHED");
    let (recovered, _) = cycle_options(
        root.path(),
        original.clone(),
        false,
        EditedNaming::Primary,
        make_options(replay.clone()),
    )
    .await;
    assert!(recovered.can_advance_database_checkpoint(), "{recovered:?}");
    assert_eq!(*replay.cursors.lock().unwrap(), [cursor]);
    assert_hold(root.path(), "cycle-token", false, false);
    assert_bytes(&path, local_sidecar);
    let before = media(root.path());
    let (steady, _) = cycle_options(
        root.path(),
        original,
        true,
        EditedNaming::Primary,
        make_options(readiness("FINISHED")),
    )
    .await;
    assert!(steady.can_advance_database_checkpoint(), "{steady:?}");
    assert_eq!(steady.stats.downloaded, 0);
    assert_eq!(media(root.path()), before);
}
