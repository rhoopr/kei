//! Managed indexing recovery uses real readiness, reopened state and file owners.
use super::{
    CycleOptions, EDIT_ONE, EditedNaming, IndexingOptions, ORIGINAL, assert_bytes, cycle,
    cycle_options, db_count, records, server, try_cycle_options,
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
fn with_omitted(server: &wiremock::MockServer) -> Vec<Value> {
    let mut all = records(server, Some(("edit-one", EDIT_ONE)));
    let mut omitted = records(server, None);
    for record in &mut omitted {
        let name = record["recordName"].as_str().unwrap().replace("501", "502");
        record["recordName"] = json!(name);
        if let Some(reference) = record["fields"].get_mut("masterRef") {
            reference["value"]["recordName"] = json!("502");
        }
    }
    omitted[0]["fields"]["filenameEnc"]["value"] = json!("IMG_0502.JPG");
    all.extend(omitted);
    all
}

#[tokio::test]
async fn indexing_managed_reopen_qualifies_then_materializes_omissions_with_quiet_feed() {
    for naming in [EditedNaming::Primary, EditedNaming::Suffix] {
        let server = server().await;
        let root = tempfile::tempdir().unwrap();
        let (initial, _) = cycle(
            root.path(),
            records(&server, None),
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
            let error = try_cycle_options(root.path(), vec![], true, naming, options(&running))
                .await
                .unwrap_err();
            assert!(
                error.to_string().contains("Unqualified indexing recovery"),
                "{error:#}"
            );
            assert_hold(root.path(), &cursor, true, false);
            assert_eq!(media(root.path()), before_media);
            assert_eq!(managed_state(root.path()), before_state);
        }
        let all = with_omitted(&server);
        let finished = readiness("FINISHED");
        let (inventory, lookups) =
            cycle_options(root.path(), all.clone(), true, naming, options(&finished)).await;
        assert_eq!(inventory.failed_count, 0, "{:?}", inventory.stats);
        assert!(!inventory.can_advance_database_checkpoint());
        assert_hold(root.path(), &cursor, false, true);
        assert!(metadata(root.path(), &indexing_replay_key(ZONE)).is_some());
        assert!(finished.cursors.lock().unwrap().is_empty());
        assert_eq!(
            lookups, 0,
            "qualification must not run managed current confirmation"
        );
        assert_eq!(server.received_requests().await.unwrap().len(), gets);
        assert_eq!(media(root.path()), before_media);
        assert_eq!(managed_state(root.path()), before_state);
        let materialize = readiness("FINISHED");
        let mut retry = readiness("FINISHED");
        retry.retry_only = true;
        let (retry_result, _) =
            cycle_options(root.path(), all.clone(), true, naming, options(&retry)).await;
        assert!(!retry_result.can_advance_database_checkpoint());
        assert_hold(root.path(), &cursor, false, true);
        let (recovered, _) = cycle_options(
            root.path(),
            all.clone(),
            true,
            naming,
            options(&materialize),
        )
        .await;
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
        assert_eq!(*materialize.cursors.lock().unwrap(), [cursor]);
        assert_hold(root.path(), "cycle-token", false, false);
        assert!(metadata(root.path(), &indexing_replay_key(ZONE)).is_none());
        let target = root.path().join("media");
        assert_bytes(&target.join("IMG_0502.JPG"), ORIGINAL);
        assert_bytes(
            &target.join(if naming == EditedNaming::Primary {
                "IMG_0501.JPG"
            } else {
                "IMG_0501_edited.JPG"
            }),
            EDIT_ONE,
        );
        assert_bytes(
            &target.join(if naming == EditedNaming::Primary {
                "IMG_0501_original.JPG"
            } else {
                "IMG_0501.JPG"
            }),
            ORIGINAL,
        );
        let gets = server.received_requests().await.unwrap().len();
        for _ in 0..2 {
            let steady = readiness("FINISHED");
            let (result, _) =
                cycle_options(root.path(), all.clone(), true, naming, options(&steady)).await;
            assert!(result.can_advance_database_checkpoint());
            assert_eq!(result.stats.downloaded, 0);
            assert_eq!(steady.queries.load(std::sync::atomic::Ordering::SeqCst), 0);
            assert_eq!(server.received_requests().await.unwrap().len(), gets);
        }
    }
}

#[tokio::test]
async fn indexing_managed_inventory_faults_never_publish_or_release_hold() {
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
        cycle(
            root.path(),
            records(&server, None),
            false,
            EditedNaming::Primary,
        )
        .await;
        let cursor = metadata(root.path(), CURSOR).unwrap();
        let running = readiness("RUNNING");
        assert!(
            try_cycle_options(
                root.path(),
                vec![],
                true,
                EditedNaming::Primary,
                options(&running)
            )
            .await
            .is_err()
        );
        let before_media = media(root.path());
        let before_state = managed_state(root.path());
        let gets = server.received_requests().await.unwrap().len();
        let mut current = records(&server, Some(("edit-one", EDIT_ONE)));
        let mut finished = readiness("FINISHED");
        match fault {
            "regression" => finished.regress = true,
            "cancel" => finished.interrupt = true,
            "bound" => finished.recent = Some(1),
            "stale" => finished.stale_plan = true,
            "identity" => current[1]["recordName"] = json!(""),
            "token" => finished.token = None,
            "write" => {
                rusqlite::Connection::open(root.path().join("state.db")).unwrap().execute_batch("CREATE TRIGGER fail_indexing_qualification BEFORE INSERT ON metadata WHEN NEW.key LIKE 'indexing_materialization_required:%' BEGIN SELECT RAISE(FAIL,'injected qualification failure'); END").unwrap();
            }
            _ => panic!("unknown fixture fault: {fault}"),
        }
        let (result, _) = cycle_options(
            root.path(),
            current,
            true,
            EditedNaming::Primary,
            options(&finished),
        )
        .await;
        assert!(
            !result.can_advance_database_checkpoint(),
            "{fault}: {:?}",
            result.stats
        );
        assert_hold(root.path(), &cursor, true, false);
        assert!(
            metadata(root.path(), &indexing_replay_key(ZONE)).is_none(),
            "atomic transition: {fault}"
        );
        assert_eq!(media(root.path()), before_media, "{fault}");
        assert_eq!(managed_state(root.path()), before_state, "{fault}");
        assert_eq!(
            server.received_requests().await.unwrap().len(),
            gets,
            "{fault}"
        );
    }
}

#[tokio::test]
async fn indexing_managed_materialization_failure_keeps_full_debt_and_cursor() {
    for fault in ["transfer", "commit", "cancel", "bound", "regression"] {
        let server = server().await;
        let root = tempfile::tempdir().unwrap();
        cycle(
            root.path(),
            records(&server, None),
            false,
            EditedNaming::Primary,
        )
        .await;
        let cursor = metadata(root.path(), CURSOR).unwrap();
        let running = readiness("RUNNING");
        assert!(
            try_cycle_options(
                root.path(),
                vec![],
                true,
                EditedNaming::Primary,
                options(&running)
            )
            .await
            .is_err()
        );
        let current = records(&server, Some(("edit-one", EDIT_ONE)));
        cycle_options(
            root.path(),
            current.clone(),
            true,
            EditedNaming::Primary,
            options(&readiness("FINISHED")),
        )
        .await;
        assert_hold(root.path(), &cursor, false, true);
        let mut interrupted = readiness("FINISHED");
        let mut bad = current.clone();
        match fault {
            "transfer" => {
                bad = records(&server, Some(("unavailable", EDIT_ONE)));
            }
            "commit" => {
                rusqlite::Connection::open(root.path().join("state.db")).unwrap().execute_batch("CREATE TRIGGER fail_recovery_checkpoint BEFORE UPDATE OF value ON metadata WHEN NEW.key='sync_token:PrimarySync' BEGIN SELECT RAISE(FAIL,'injected recovery checkpoint failure'); END").unwrap();
            }
            "cancel" => interrupted.interrupt = true,
            "bound" => interrupted.recent = Some(1),
            "regression" => interrupted.regress = true,
            _ => panic!("unknown fixture fault: {fault}"),
        }
        let (held, _) = cycle_options(
            root.path(),
            bad,
            true,
            EditedNaming::Primary,
            options(&interrupted),
        )
        .await;
        assert!(
            !held.can_advance_database_checkpoint(),
            "{fault}: {:?}",
            held.stats
        );
        let full_hold = fault == "regression";
        assert_eq!(
            metadata(root.path(), &indexing_reconciliation_key(ZONE)).is_some(),
            full_hold,
            "{fault}: {:?}",
            held.stats
        );
        assert_hold(root.path(), &cursor, full_hold, true);
        assert!(metadata(root.path(), &indexing_replay_key(ZONE)).is_some());
        if fault == "commit" {
            rusqlite::Connection::open(root.path().join("state.db"))
                .unwrap()
                .execute_batch("DROP TRIGGER fail_recovery_checkpoint")
                .unwrap();
        }
        // A late RUNNING observation starts qualification again, while the
        // full materialization obligation survives both transactions/reopens.
        if full_hold {
            cycle_options(
                root.path(),
                current.clone(),
                true,
                EditedNaming::Primary,
                options(&readiness("FINISHED")),
            )
            .await;
            assert_hold(root.path(), &cursor, false, true);
        }
        let (repaired, _) = cycle_options(
            root.path(),
            current,
            true,
            EditedNaming::Primary,
            options(&readiness("FINISHED")),
        )
        .await;
        assert_eq!(repaired.failed_count, 0, "{fault}: {:?}", repaired.stats);
        assert!(
            repaired.can_advance_database_checkpoint(),
            "{fault}: {:?}",
            repaired.stats
        );
        assert_hold(root.path(), "cycle-token", false, false);
        assert_bytes(&root.path().join("media/IMG_0501.JPG"), EDIT_ONE);
        assert_bytes(&root.path().join("media/IMG_0501_original.JPG"), ORIGINAL);
    }
}

#[tokio::test]
async fn indexing_managed_pending_journal_is_held_even_under_suffix_then_recovers() {
    let server = server().await;
    let root = tempfile::tempdir().unwrap();
    drop(
        crate::state::SqliteStateDb::open(&root.path().join("state.db"))
            .await
            .unwrap(),
    );
    let conn = rusqlite::Connection::open(root.path().join("state.db")).unwrap();
    conn.execute(
        "INSERT INTO metadata(key,value) VALUES(?1,'trusted-before')",
        [CURSOR],
    )
    .unwrap();
    conn.execute_batch("CREATE TRIGGER fail_layout_commit BEFORE INSERT ON primary_layout_bindings BEGIN SELECT RAISE(FAIL,'injected journal commit failure'); END").unwrap();
    drop(conn);
    let current = records(&server, Some(("edit-one", EDIT_ONE)));
    let (interrupted, _) = cycle(root.path(), current.clone(), false, EditedNaming::Primary).await;
    assert!(!interrupted.can_advance_database_checkpoint());
    assert_eq!(
        db_count(root.path(), "SELECT COUNT(*) FROM primary_layout_bindings"),
        0
    );
    assert_eq!(
        db_count(
            root.path(),
            "SELECT COUNT(*) FROM primary_layout_operations WHERE phase='publishing'"
        ),
        1
    );
    let before_media = media(root.path());
    let before_state = managed_state(root.path());
    for _ in 0..2 {
        let error = try_cycle_options(
            root.path(),
            vec![],
            true,
            EditedNaming::Suffix,
            options(&readiness("RUNNING")),
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("Unqualified indexing recovery"));
        assert_eq!(media(root.path()), before_media);
        assert_eq!(managed_state(root.path()), before_state);
    }
    let (inventory, _) = cycle_options(
        root.path(),
        current.clone(),
        true,
        EditedNaming::Suffix,
        options(&readiness("FINISHED")),
    )
    .await;
    assert!(!inventory.can_advance_database_checkpoint());
    assert_hold(root.path(), "trusted-before", false, true);
    assert_eq!(media(root.path()), before_media);
    assert_eq!(managed_state(root.path()), before_state);
    rusqlite::Connection::open(root.path().join("state.db"))
        .unwrap()
        .execute_batch("DROP TRIGGER fail_layout_commit")
        .unwrap();
    let (recovered, _) = cycle_options(
        root.path(),
        current,
        true,
        EditedNaming::Suffix,
        options(&readiness("FINISHED")),
    )
    .await;
    assert_eq!(recovered.failed_count, 0, "{:?}", recovered.stats);
    assert!(recovered.can_advance_database_checkpoint());
    assert_hold(root.path(), "cycle-token", false, false);
    assert_eq!(
        db_count(
            root.path(),
            "SELECT COUNT(*) FROM primary_layout_operations WHERE phase NOT IN ('committed','cancelled')"
        ),
        0
    );
    assert_bytes(&root.path().join("media/IMG_0501.JPG"), ORIGINAL);
    assert_bytes(&root.path().join("media/IMG_0501_edited.JPG"), EDIT_ONE);
}

#[tokio::test]
async fn indexing_managed_local_modification_holds_materialization_and_foreign_collision_is_preserved()
 {
    let server = server().await;
    let root = tempfile::tempdir().unwrap();
    cycle(
        root.path(),
        records(&server, None),
        false,
        EditedNaming::Primary,
    )
    .await;
    let cursor = metadata(root.path(), CURSOR).unwrap();
    let running = readiness("RUNNING");
    assert!(
        try_cycle_options(
            root.path(),
            vec![],
            true,
            EditedNaming::Primary,
            options(&running)
        )
        .await
        .is_err()
    );
    let current = with_omitted(&server);
    let target = root.path().join("media");
    let local = b"independent local modification";
    let foreign = b"independent foreign collision";
    std::fs::write(target.join("IMG_0501.JPG"), local).unwrap();
    std::fs::write(target.join("IMG_0502.JPG"), foreign).unwrap();
    let (inventory, _) = cycle_options(
        root.path(),
        current.clone(),
        true,
        EditedNaming::Primary,
        options(&readiness("FINISHED")),
    )
    .await;
    assert_eq!(inventory.failed_count, 0);
    assert_hold(root.path(), &cursor, false, true);
    for _ in 0..2 {
        let (held, _) = cycle_options(
            root.path(),
            current.clone(),
            true,
            EditedNaming::Primary,
            options(&readiness("FINISHED")),
        )
        .await;
        assert!(!held.can_advance_database_checkpoint(), "{:?}", held.stats);
        assert_hold(root.path(), &cursor, false, true);
        assert_bytes(&target.join("IMG_0501.JPG"), local);
        assert_bytes(&target.join("IMG_0502.JPG"), foreign);
    }
    // Test-fixture operator restores exactly the original owned bytes.
    std::fs::write(target.join("IMG_0501.JPG"), ORIGINAL).unwrap();
    let (recovered, _) = cycle_options(
        root.path(),
        current,
        true,
        EditedNaming::Primary,
        options(&readiness("FINISHED")),
    )
    .await;
    assert_eq!(recovered.failed_count, 0, "{:?}", recovered.stats);
    assert!(recovered.can_advance_database_checkpoint());
    assert_hold(root.path(), "cycle-token", false, false);
    assert_bytes(&target.join("IMG_0501.JPG"), EDIT_ONE);
    assert_bytes(&target.join("IMG_0502.JPG"), foreign);
    assert!(
        media(root.path()).into_iter().any(|(path, bytes)| path
            .to_string_lossy()
            .starts_with("IMG_0502-")
            && bytes == ORIGINAL)
    );
}

#[cfg(feature = "xmp")]
#[tokio::test]
async fn indexing_managed_sidecar_modification_remains_protected_until_restored() {
    let server = super::server_with_media(super::VALID_ORIGINAL, super::VALID_EDIT, EDIT_ONE).await;
    let root = tempfile::tempdir().unwrap();
    let make_options = |indexing: IndexingOptions| CycleOptions {
        indexing: Some(indexing),
        sidecar: true,
        ..CycleOptions::default()
    };
    let original = super::records_with_original(&server, super::VALID_ORIGINAL, None);
    cycle_options(
        root.path(),
        original,
        false,
        EditedNaming::Primary,
        make_options(readiness("FINISHED")),
    )
    .await;
    let cursor = metadata(root.path(), CURSOR).unwrap();
    assert!(
        try_cycle_options(
            root.path(),
            vec![],
            true,
            EditedNaming::Primary,
            make_options(readiness("RUNNING"))
        )
        .await
        .is_err()
    );
    let path = root.path().join("media/IMG_0501.JPG.xmp");
    let trusted_sidecar = std::fs::read(&path).unwrap();
    let local_sidecar = b"independent local sidecar contents";
    std::fs::write(&path, local_sidecar).unwrap();
    let current = super::records_with_original(
        &server,
        super::VALID_ORIGINAL,
        Some(("edit-one", super::VALID_EDIT)),
    );
    cycle_options(
        root.path(),
        current.clone(),
        true,
        EditedNaming::Primary,
        make_options(readiness("FINISHED")),
    )
    .await;
    assert_hold(root.path(), &cursor, false, true);
    for _ in 0..2 {
        let (held, _) = cycle_options(
            root.path(),
            current.clone(),
            true,
            EditedNaming::Primary,
            make_options(readiness("FINISHED")),
        )
        .await;
        assert!(!held.can_advance_database_checkpoint());
        assert_hold(root.path(), &cursor, false, true);
        assert_bytes(&path, local_sidecar);
        assert_bytes(
            &root.path().join("media/IMG_0501.JPG"),
            super::VALID_ORIGINAL,
        );
    }
    std::fs::write(&path, trusted_sidecar).unwrap();
    let (recovered, _) = cycle_options(
        root.path(),
        current.clone(),
        true,
        EditedNaming::Primary,
        make_options(readiness("FINISHED")),
    )
    .await;
    assert_eq!(recovered.failed_count, 0, "{:?}", recovered.stats);
    assert!(recovered.can_advance_database_checkpoint());
    assert_hold(root.path(), "cycle-token", false, false);
    assert_bytes(&root.path().join("media/IMG_0501.JPG"), super::VALID_EDIT);
    assert_bytes(
        &root.path().join("media/IMG_0501_original.JPG"),
        super::VALID_ORIGINAL,
    );
    let before = media(root.path());
    let gets = server.received_requests().await.unwrap().len();
    let (steady, _) = cycle_options(
        root.path(),
        current,
        true,
        EditedNaming::Primary,
        make_options(readiness("FINISHED")),
    )
    .await;
    assert!(steady.can_advance_database_checkpoint());
    assert_eq!(media(root.path()), before);
    assert_eq!(server.received_requests().await.unwrap().len(), gets);
}
