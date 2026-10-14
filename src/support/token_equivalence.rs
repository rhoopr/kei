//! Strict bounded revalidation of token relationships, at capture and export.
use serde_json::{Map, Value, json};

pub(crate) const MAX_PASSES: usize = 64;

fn ordinal(value: &Value, maximum: u64) -> bool {
    value.as_u64().is_some_and(|n| n > 0 && n <= maximum)
}

fn row(value: &Value) -> Option<Value> {
    let source = value.as_object()?;
    let pass = source.get("pass")?;
    if !ordinal(pass, MAX_PASSES as u64) {
        return None;
    }
    let mut out = Map::new();
    out.insert("pass".into(), pass.clone());
    for (key, allowed) in [
        ("pass_kind", &["album", "smart_folder", "unfiled"][..]),
        (
            "query_kind",
            &[
                "library_wide",
                "album_membership",
                "smart_album",
                "burst",
                "hidden",
                "deleted",
                "other",
            ][..],
        ),
        (
            "scope",
            &[
                "unavailable",
                "different_selected_zone",
                "same_first_pass",
                "different_first_pass",
            ][..],
        ),
    ] {
        let label = source.get(key)?.as_str()?;
        if !allowed.contains(&label) {
            return None;
        }
        out.insert(key.into(), json!(label));
    }
    for (status_key, group_key, order_key) in [
        (
            "initial_status",
            "initial_group",
            "initial_completion_order",
        ),
        (
            "recovery_status",
            "recovery_group",
            "recovery_completion_order",
        ),
    ] {
        let status = source.get(status_key)?.as_str()?;
        if ![
            "not_observed",
            "not_retried",
            "present",
            "missing",
            "blank",
            "receiver_dropped",
            "enumeration_incomplete",
            "token_too_large",
            "group_limit",
        ]
        .contains(&status)
        {
            return None;
        }
        let group = source.get(group_key)?;
        if (status == "present" && !ordinal(group, (MAX_PASSES * 2) as u64))
            || (status != "present" && !group.is_null())
        {
            return None;
        }
        let order = source.get(order_key)?;
        if !order.is_null() && !ordinal(order, u32::MAX.into()) {
            return None;
        }
        out.insert(status_key.into(), json!(status));
        out.insert(group_key.into(), group.clone());
        out.insert(order_key.into(), order.clone());
    }
    Some(Value::Object(out))
}

pub(super) fn retain_rows(source: &Map<String, Value>, out: &mut Map<String, Value>) {
    let input = source.get("rows").and_then(Value::as_array);
    let mut rejected = source
        .get("rows_rejected")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let mut seen = [false; MAX_PASSES];
    let mut rows = Vec::new();
    if let Some(input) = input {
        rejected = rejected.saturating_add(input.len().saturating_sub(MAX_PASSES) as u64);
        for value in input.iter().take(MAX_PASSES) {
            if let Some(row) = row(value)
                && let Some(index) = row
                    .get("pass")
                    .and_then(Value::as_u64)
                    .and_then(|n| usize::try_from(n - 1).ok())
                && let Some(seen) = seen.get_mut(index)
                && !*seen
            {
                *seen = true;
                rows.push(row);
            } else {
                rejected = rejected.saturating_add(1);
            }
        }
    }
    out.insert("rows".into(), json!(rows));
    out.insert("rows_rejected".into(), json!(rejected));
    out.insert(
        "rows_unavailable".into(),
        json!(input.is_none() || source.get("rows_unavailable") == Some(&Value::Bool(true))),
    );
}

#[cfg(test)]
mod tests {
    use super::MAX_PASSES;
    use crate::support::privacy;
    use serde_json::{Value, json};

    fn valid_row(pass: usize) -> Value {
        json!({"pass":pass,"pass_kind":"album","query_kind":"album_membership","scope":"same_first_pass",
            "initial_status":"present","initial_group":1,"initial_completion_order":1,
            "recovery_status":"present","recovery_group":1,"recovery_completion_order":2})
    }
    fn clean(value: &Value) -> Value {
        Value::Object(
            privacy::diagnostic("support_token_equivalence_v1", value.as_object().unwrap())
                .unwrap(),
        )
    }

    #[test]
    fn token_equivalence_privacy_roundtrip_rejects_private_and_malformed_values() {
        let mut row = valid_row(1);
        row["token"] = json!("PRIVATE_TOKEN");
        row["album"] = json!("PRIVATE_ALBUM");
        let mut input = json!({"phase":"same_cycle_recovery","passes_total":1,"passes_omitted":0,"rows":[row],"error":"PRIVATE_ERROR","owner":"PRIVATE_OWNER"});
        let safe = clean(&input);
        assert_eq!(
            safe["rows"][0]["initial_group"],
            safe["rows"][0]["recovery_group"]
        );
        assert!(!safe.to_string().contains("PRIVATE"));
        assert_eq!(clean(&safe), safe);
        for key in [
            "pass",
            "pass_kind",
            "query_kind",
            "scope",
            "initial_status",
            "initial_group",
            "initial_completion_order",
            "recovery_status",
            "recovery_group",
            "recovery_completion_order",
        ] {
            let mut malformed = input.clone();
            malformed["rows"][0][key] = json!("PRIVATE_VALUE");
            let safe = clean(&malformed);
            assert_eq!(safe["rows"], json!([]), "{key}");
            assert_eq!(safe["rows_rejected"], 1);
            assert!(!safe.to_string().contains("PRIVATE"));
            assert_eq!(clean(&safe), safe);
        }
        input["rows"][0]["initial_group"] = json!(129);
        assert_eq!(clean(&input)["rows_rejected"], 1);
        input["rows"][0]["initial_group"] = json!(null);
        assert_eq!(clean(&input)["rows_rejected"], 1);
        input["rows"][0]["initial_status"] = json!("blank");
        assert_eq!(clean(&input)["rows_rejected"], 0);
        input["rows"][0]["initial_group"] = json!(1);
        assert_eq!(clean(&input)["rows_rejected"], 1);
    }

    #[test]
    fn token_equivalence_export_bounds_and_missing_rows_are_explicit() {
        let rows = (1..=MAX_PASSES + 5).map(valid_row).collect::<Vec<_>>();
        let input = json!({"phase":"initial","passes_total":69,"passes_omitted":0,"rows":rows});
        let safe = clean(&input);
        assert_eq!(safe["rows"].as_array().unwrap().len(), MAX_PASSES);
        assert_eq!(safe["rows_rejected"], 5);
        assert_eq!(clean(&safe), safe);
        assert!(safe.to_string().len() < 64 * 1024);
        for missing in [
            json!({}),
            json!({"rows":"PRIVATE_ROWS"}),
            json!({"rows":null}),
        ] {
            let safe = clean(&missing);
            assert_eq!(safe["rows_unavailable"], true);
            assert_eq!(safe["rows"], json!([]));
            assert_eq!(clean(&safe), safe);
        }
        let duplicate = clean(&json!({"rows":[valid_row(1),valid_row(1)]}));
        assert_eq!(duplicate["rows_rejected"], 1);
        assert_eq!(duplicate["rows"].as_array().unwrap().len(), 1);
    }
}
