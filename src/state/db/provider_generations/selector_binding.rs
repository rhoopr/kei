//! Explicit selecting-owner identity for source-scoped album work.
use super::{GenerationSpec, invalid, owner_keys};
use crate::state::error::StateError;
use rusqlite::Connection;
use serde_json::Value;

pub(super) fn validate(spec: &GenerationSpec, pass: &Value) -> Result<(), StateError> {
    let Some(binding) = spec.profile.get("selector_binding") else {
        return if pass.pointer("/scope/zone") == Some(&spec.zone) {
            Ok(())
        } else {
            invalid()
        };
    };
    let source: Value = serde_json::from_str(&spec.scope)
        .map_err(|_invalid| StateError::ProviderSelectionInvalid)?;
    let scope = binding
        .get("scope")
        .ok_or(StateError::ProviderSelectionInvalid)?;
    let zone = binding
        .get("zone")
        .ok_or(StateError::ProviderSelectionInvalid)?;
    let identity = |name: &str| {
        binding
            .get(name)
            .and_then(Value::as_str)
            .is_some_and(|value| {
                let value = if name == "account_key" {
                    let Some(value) = value.strip_prefix("account-v1-") else {
                        return false;
                    };
                    value
                } else {
                    value
                };
                value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
            })
    };
    if spec.format != 2
        || binding.get("version").and_then(Value::as_u64) != Some(2)
        || !identity("account_key")
        || !identity("provider_key")
        || scope.get("format").and_then(Value::as_u64) != Some(1)
        || !matches!(
            scope.get("database").and_then(Value::as_str),
            Some("private" | "shared")
        )
        || ["realm", "container", "environment"]
            .into_iter()
            .any(|key| scope.get(key) != source.get(key))
        || ["zoneName", "ownerRecordName"].into_iter().any(|key| {
            zone.get(key)
                .and_then(Value::as_str)
                .is_none_or(str::is_empty)
                || zone.get(key) != scope.get("zone").and_then(|value| value.get(key))
        })
        || zone == &spec.zone
        || pass.pointer("/scope/zone") != Some(zone)
        || pass.get("kind").and_then(Value::as_str) != Some("Album")
        || pass
            .pointer("/scope/container_id")
            .and_then(Value::as_str)
            .is_none_or(str::is_empty)
        || !pass
            .pointer("/scope/cross_zone_sources")
            .and_then(Value::as_array)
            .is_some_and(|sources| {
                sources
                    .iter()
                    .filter(|item| item.get("zone") == Some(&spec.zone))
                    .count()
                    == 1
            })
    {
        return invalid();
    }
    Ok(())
}

pub(super) fn validate_owner(conn: &Connection, spec: &GenerationSpec) -> Result<(), StateError> {
    let Some(binding) = spec.profile.get("selector_binding") else {
        return Ok(());
    };
    let (account, provider) = owner_keys(conn)?;
    if binding.get("account_key").and_then(Value::as_str) != Some(account.as_str())
        || binding.get("provider_key").and_then(Value::as_str) != Some(provider.as_str())
    {
        return invalid();
    }
    Ok(())
}

/// Structural authority retained with an unfinished destination. Freshness is
/// checked by the selecting owner before admission; source bytes alone cannot
/// authorize another album's destination.
pub(super) fn validate_membership(
    spec: &GenerationSpec,
    pass: &Value,
    child: &str,
    body: &[u8],
) -> Result<(), StateError> {
    let Some(binding) = spec.profile.get("selector_binding") else {
        return Ok(());
    };
    let value: Value =
        serde_json::from_slice(body).map_err(|_invalid| StateError::ProviderSelectionInvalid)?;
    let witness = value
        .get("selector_membership")
        .ok_or(StateError::ProviderSelectionInvalid)?;
    if witness.get("version").and_then(Value::as_u64) != Some(2)
        || !matches!(
            witness.get("kind").and_then(Value::as_str),
            Some("complete_relation_scan" | "relation_observation")
        )
        || witness.get("source_zone") != Some(&spec.zone)
        || witness.get("source_scope").and_then(Value::as_str) != Some(spec.scope.as_str())
        || witness
            .get("selector_child")
            .and_then(Value::as_str)
            .is_none_or(|value| value.trim().is_empty())
        || witness.get("selector_zone") != binding.get("zone")
        || witness.get("container") != pass.pointer("/scope/container_id")
        || witness.get("child").and_then(Value::as_str) != Some(child)
        || witness
            .get("relation")
            .and_then(Value::as_str)
            .is_none_or(|value| value.trim().is_empty())
    {
        return invalid();
    }
    Ok(())
}

pub(super) fn compatible(previous: &Value, current: &Value) -> bool {
    previous.get("selector_binding") == current.get("selector_binding")
}

#[cfg(test)]
mod tests {
    use super::{compatible, validate, validate_membership, validate_owner};
    use crate::state::db::provider_generations::GenerationSpec;
    use serde_json::{Value, json};

    fn fixture() -> (GenerationSpec, Value) {
        let source_zone = json!({"zoneName":"Shared","ownerRecordName":"shared-owner"});
        let selector_zone = json!({"zoneName":"Primary","ownerRecordName":"_defaultOwner"});
        let scope = |database, zone| json!({"format":1,"realm":"com","container":"com.apple.photos.cloud","environment":"production","database":database,"zone":zone});
        let pass = json!({"key":"selected-pass","kind":"Album","scope":{"zone":selector_zone,"container_id":"album-a","cross_zone_sources":[{"zone":source_zone}]}});
        let spec = GenerationSpec {
            format: 2,
            scope: scope("shared", source_zone.clone()).to_string(),
            zone: source_zone,
            config_hash: "a".repeat(64),
            basis: "b".repeat(64),
            metadata_enabled: false,
            metadata_flags: 0,
            profile: json!({"coverage":"selected_downloads","passes":[pass.clone()],"selector_binding":{
                "version":2,"account_key":format!("account-v1-{}", "a".repeat(64)),"provider_key":"b".repeat(64),
                "scope":scope("private",selector_zone.clone()),"zone":selector_zone}}),
        };
        (spec, pass)
    }

    #[test]
    fn explicit_selector_keeps_both_scopes_and_rejects_unknown_versions() {
        let (spec, pass) = fixture();
        validate(&spec, &pass).unwrap();
        let encoded = serde_json::to_vec(&spec).unwrap();
        let reopened: GenerationSpec = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(reopened, spec);
        validate(&reopened, &pass).unwrap();
        for version in [0, 1, 99] {
            let mut changed = spec.clone();
            changed.profile["selector_binding"]["version"] = json!(version);
            assert!(validate(&changed, &pass).is_err());
        }
        let mut old = spec.clone();
        old.profile
            .as_object_mut()
            .unwrap()
            .remove("selector_binding");
        assert!(
            validate(&old, &pass).is_err(),
            "absence cannot silently reinterpret a cross-source pass"
        );
        let mut same_source = pass.clone();
        same_source["scope"]["zone"] = spec.zone.clone();
        validate(&old, &same_source).unwrap();
    }

    #[test]
    fn selector_binding_fences_account_source_and_retry_compatibility() {
        let (spec, pass) = fixture();
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE account_owner(singleton INTEGER,account_key TEXT,provider_key TEXT);",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO account_owner VALUES(1,?1,?2)",
            rusqlite::params![format!("account-v1-{}", "a".repeat(64)), "b".repeat(64)],
        )
        .unwrap();
        validate_owner(&conn, &spec).unwrap();
        for key in ["account_key", "provider_key"] {
            let mut changed = spec.clone();
            changed.profile["selector_binding"][key] = json!("c".repeat(64));
            assert!(validate_owner(&conn, &changed).is_err());
            assert!(!compatible(&spec.profile, &changed.profile));
        }
        let mut foreign = pass.clone();
        foreign["scope"]["cross_zone_sources"][0]["zone"]["ownerRecordName"] = json!("foreign");
        assert!(validate(&spec, &foreign).is_err());
        let mut missing = pass.clone();
        missing["scope"]["container_id"] = Value::Null;
        assert!(validate(&spec, &missing).is_err());
        let mut changed = spec.profile.clone();
        changed["selector_binding"]["scope"]["database"] = json!("shared");
        assert!(!compatible(&spec.profile, &changed));
        assert!(
            compatible(&json!({"passes":[]}), &json!({"passes":[1]})),
            "old compatibility is unchanged"
        );
    }
    #[test]
    fn selecting_membership_is_required_and_bound_to_pass_child_and_zone() {
        let (spec, pass) = fixture();
        let witness = json!({"version":2,"kind":"complete_relation_scan", "selector_zone":spec.profile["selector_binding"]["zone"],
            "source_zone":spec.zone,"source_scope":spec.scope,"selector_child":"owner-asset-a",
            "container":"album-a","child":"asset-a","relation":"relation-a"});
        let body = json!({"selector_membership":witness});
        validate_membership(&spec, &pass, "asset-a", &serde_json::to_vec(&body).unwrap()).unwrap();
        assert!(validate_membership(&spec, &pass, "asset-a", b"{}").is_err());
        for (key, value) in [
            ("version", json!(1)),
            ("kind", json!("unknown")),
            ("container", json!("album-b")),
            ("child", json!("asset-b")),
            ("relation", json!("")),
            ("selector_zone", json!({"zoneName":"foreign"})),
        ] {
            let mut changed = body.clone();
            changed["selector_membership"][key] = value;
            assert!(
                validate_membership(
                    &spec,
                    &pass,
                    "asset-a",
                    &serde_json::to_vec(&changed).unwrap()
                )
                .is_err(),
                "{key}"
            );
        }
        let mut current = body;
        current["selector_membership"]["kind"] = json!("relation_observation");
        validate_membership(
            &spec,
            &pass,
            "asset-a",
            &serde_json::to_vec(&current).unwrap(),
        )
        .unwrap();
    }
}
