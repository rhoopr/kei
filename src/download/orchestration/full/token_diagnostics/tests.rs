use std::sync::Arc;

use futures_util::{StreamExt, stream};
use serde_json::json;

use super::{CompletionOrder, MAX_PASSES, MAX_TOKEN_BYTES, TokenDiagnostics};
use crate::commands::{AlbumPass, PassKind};
use crate::download::orchestration::full::{PassTokenObservation, PassTokenResult};
use crate::download::orchestration::models::PassKey;
use crate::icloud::photos::PhotoAlbum;

fn passes(count: usize) -> Vec<AlbumPass> {
    (0..count)
        .map(|_| AlbumPass {
            kind: PassKind::Album,
            album: PhotoAlbum::stub_for_test(Arc::from("PRIVATE_ALBUM")),
            exclude_ids: Arc::default(),
        })
        .collect()
}

fn observation(index: usize, result: PassTokenResult) -> PassTokenObservation {
    PassTokenObservation {
        pass: PassKey {
            index,
            kind: PassKind::Album,
            label: "PRIVATE_LABEL".into(),
        },
        result,
    }
}

#[test]
fn token_equivalence_equal_across_rounds_gaps_and_bounds() {
    let mut evidence = TokenDiagnostics::new(&passes(MAX_PASSES + 3), "PrimarySync");
    for index in 0..MAX_PASSES + 3 {
        evidence.record(
            &observation(
                index,
                PassTokenResult::Present(format!("PRIVATE_TOKEN_{index}")),
            ),
            false,
        );
        evidence.recovery_requested(index);
        evidence.record(
            &observation(
                index,
                PassTokenResult::Present(format!("PRIVATE_TOKEN_{index}")),
            ),
            true,
        );
    }
    let fields = evidence.fields("same_cycle_recovery");
    assert_eq!(fields["passes_omitted"], 3);
    assert_eq!(fields["rows"].as_array().unwrap().len(), MAX_PASSES);
    assert_eq!(evidence.tokens.len(), MAX_PASSES);
    for row in fields["rows"].as_array().unwrap() {
        assert_eq!(row["initial_group"], row["recovery_group"]);
    }
    let cases = [
        (PassTokenResult::Missing, "missing"),
        (PassTokenResult::Blank, "blank"),
        (PassTokenResult::ReceiverDropped, "receiver_dropped"),
        (
            PassTokenResult::EnumerationIncomplete,
            "enumeration_incomplete",
        ),
        (
            PassTokenResult::Present("X".repeat(MAX_TOKEN_BYTES + 1)),
            "token_too_large",
        ),
    ];
    for (index, (result, status)) in cases.into_iter().enumerate() {
        evidence.record(&observation(index, result), true);
        assert_eq!(evidence.rows[index]["recovery_status"], status);
        assert_eq!(evidence.rows[index]["recovery_group"], json!(null));
        assert_eq!(
            evidence.rows[index]["recovery_completion_order"],
            json!(null)
        );
    }
    assert!(
        !evidence
            .fields("same_cycle_recovery")
            .to_string()
            .contains("PRIVATE")
    );
    // A new invocation has a fresh local equivalence table, regardless of values.
    let mut fresh = TokenDiagnostics::new(&passes(1), "PrimarySync");
    fresh.record(
        &observation(0, PassTokenResult::Present("UNRELATED_TOKEN".into())),
        false,
    );
    assert_eq!(fresh.rows[0]["initial_group"], 1);
    assert_eq!(fresh.rows[0]["recovery_status"], "not_retried");
    assert_eq!(TokenDiagnostics::new(&[], "PrimarySync").rows.len(), 0);
}

#[tokio::test]
async fn token_equivalence_completion_is_drain_order_not_registration_order() {
    let order = CompletionOrder::default();
    let first = order.wrap(Box::pin(stream::empty()), 0);
    let mut second = order.wrap(Box::pin(stream::empty()), 1);
    assert!(second.next().await.is_none());
    assert!(second.next().await.is_none());
    assert_eq!(order.get(1), Some(1));
    drop(first);
    assert_eq!(order.get(0), None);
    let mut third = order.wrap(Box::pin(stream::empty()), 2);
    assert!(third.next().await.is_none());
    assert_eq!(order.get(2), Some(2));
}

#[test]
fn token_equivalence_group_table_is_bounded_across_distinct_rounds() {
    let mut selected = passes(MAX_PASSES);
    selected[1].kind = PassKind::SmartFolder;
    selected[2].kind = PassKind::Unfiled;
    let mut evidence = TokenDiagnostics::new(&selected, "PrimarySync");
    for index in 0..MAX_PASSES {
        evidence.record(
            &observation(index, PassTokenResult::Present(format!("INITIAL_{index}"))),
            false,
        );
        evidence.record(
            &observation(index, PassTokenResult::Present(format!("RECOVERY_{index}"))),
            true,
        );
    }
    assert_eq!(evidence.tokens.len(), MAX_PASSES * 2);
    let rows = &evidence.rows;
    assert_eq!(rows[0]["pass_kind"], "album");
    assert_eq!(rows[1]["pass_kind"], "smart_folder");
    assert_eq!(rows[2]["pass_kind"], "unfiled");
    for row in rows {
        assert_ne!(row["initial_group"], row["recovery_group"]);
    }
    // Defensive overflow must remain missing evidence, never an invented group.
    evidence.record(
        &observation(0, PassTokenResult::Present("EXTRA_VALUE".into())),
        true,
    );
    assert_eq!(evidence.rows[0]["recovery_status"], "group_limit");
    assert_eq!(evidence.rows[0]["recovery_group"], json!(null));
    assert_eq!(evidence.tokens.len(), MAX_PASSES * 2);
    evidence.record(
        &observation(0, PassTokenResult::Present("INITIAL_0".into())),
        true,
    );
    assert_eq!(
        evidence.rows[0]["recovery_group"],
        evidence.rows[0]["initial_group"]
    );
}
