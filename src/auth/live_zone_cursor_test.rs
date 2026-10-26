//! Opt-in saved-session Photos cursor probe. Diagnostic errors/logs omit bodies/tokens.
//! Opening the saved session may perform ordinary local cookie-file maintenance.

use serde_json::Value;

pub(crate) async fn existing_session_for_validation()
-> Result<(super::session::Session, String), &'static str> {
    let (session, root, _) = existing_session_and_owner_for_validation().await?;
    Ok((session, root))
}

pub(crate) async fn existing_session_and_owner_for_validation() -> Result<
    (
        super::session::Session,
        String,
        crate::state::db::account::AccountOwner,
    ),
    &'static str,
> {
    if std::env::var("KEI_SESSION_CURSOR_PROBE").as_deref() != Ok("1") {
        return Err("explicit session probe opt-in is missing");
    }
    let env_path = std::env::var("KEI_SESSION_PROBE_ENV")
        .map_err(|_error| "documented test environment path missing")?;
    dotenvy::from_path(env_path).map_err(|_error| "documented test environment unavailable")?;
    let username =
        std::env::var("ICLOUD_USERNAME").map_err(|_error| "test username unavailable")?;
    let cookie_dir = std::env::var("KEI_SESSION_PROBE_COOKIE_DIR")
        .map_err(|_error| "existing test session directory missing")?;
    let endpoints =
        super::Endpoints::for_domain("com").map_err(|_error| "test realm unavailable")?;
    let mut session = super::session::Session::new(
        std::path::Path::new(&cookie_dir),
        &username,
        endpoints.home,
        Some(30),
    )
    .await
    .map_err(|_error| "existing session could not be opened; no login attempted")?;
    if !session.loaded_persisted_auth() {
        return Err("saved authenticated session was not loaded; no login attempted");
    }
    // Use only /validate. In particular, do not call authenticate(), whose
    // fallback can start SRP or 2FA, or validate_session()'s accountLogin fallback.
    let response = session
        .http_client()
        .post(format!("{}/validate", endpoints.setup))
        .header("Origin", endpoints.home)
        .header("Referer", format!("{}/", endpoints.home))
        .body("null")
        .send()
        .await
        .map_err(|_error| "session validation request failed; no login attempted")?;
    if !response.status().is_success() {
        return Err("saved session validation rejected; no login attempted");
    }
    let data: super::AccountLoginResponse = response
        .json()
        .await
        .map_err(|_error| "session validation response invalid")?;
    if super::check_requires_2fa(&data) || data.has_error || data.i_cdp_enabled {
        return Err("saved session is not usable for this read-only check; no login attempted");
    }
    session
        .bind_principal(&data, None)
        .map_err(|_error| "authenticated principal validation failed")?;
    let root = data
        .webservices
        .as_ref()
        .and_then(|services| services.ckdatabasews.as_ref())
        .map(|endpoint| endpoint.url.clone())
        .ok_or("validated Photos endpoint missing")?;
    let owner = crate::state::db::account::AccountOwner::authenticated(&username, "com", &data)
        .map_err(|_error| "authenticated state owner unavailable")?;
    Ok((session, root, owner))
}

// This only identifies candidates for the bounded diagnostic; it is not
// production library discovery and cannot establish snapshot consistency.
fn photos_cursor_candidate(zone: &Value) -> Result<Option<(&Value, &str)>, &'static str> {
    if zone.get("deleted").and_then(Value::as_bool) == Some(true) {
        return Ok(None);
    }
    if zone
        .get("serverErrorCode")
        .is_some_and(|value| !value.is_null())
    {
        return Err("provider returned a zone-list error");
    }
    let zone_id = zone.get("zoneID").ok_or("provider zone identity missing")?;
    if !zone_id
        .get("zoneName")
        .and_then(Value::as_str)
        .is_some_and(|name| name == "PrimarySync" || name.starts_with("SharedSync-"))
    {
        return Ok(None);
    }
    Ok(zone
        .get("syncToken")
        .and_then(Value::as_str)
        .filter(|token| !token.trim().is_empty())
        .map(|token| (zone_id, token)))
}

fn cursor_response_matches_scope(response: &Value, zone_id: &Value) -> bool {
    let Some(returned) = response.get("zones").and_then(Value::as_array) else {
        return false;
    };
    returned.len() == 1
        && returned[0].get("zoneID") == Some(zone_id)
        && returned[0]
            .get("serverErrorCode")
            .is_none_or(Value::is_null)
        && returned[0]
            .get("syncToken")
            .and_then(Value::as_str)
            .is_some_and(|value| !value.trim().is_empty())
}

#[tokio::test]
#[ignore = "requires explicit authorization and an existing authenticated test session"]
async fn existing_session_zone_cursor_is_accepted_without_reauthentication()
-> Result<(), &'static str> {
    let (session, root) = existing_session_for_validation().await?;
    eprintln!("session_validation=accepted reauthentication_attempted=false");
    let mut accepted_total = 0;
    for database in ["private", "shared"] {
        // Same endpoint layout as PhotosService::build_service_endpoint.
        let endpoint = format!("{root}/database/1/com.apple.photos.cloud/production/{database}");
        let raw = crate::icloud::photos::session::diagnostic_bytes(
            session.http_client(),
            &format!("{endpoint}/zones/list"),
            "{}".to_owned(),
            &[("Content-type", "text/plain")],
            1024 * 1024,
        )
        .await
        .map_err(|_error| "scoped zone-list read failed")?;
        let listing: Value =
            serde_json::from_slice(&raw).map_err(|_error| "scoped zone-list response invalid")?;
        let zones = listing
            .get("zones")
            .and_then(Value::as_array)
            .ok_or("scoped zone list missing")?;
        if listing
            .get("moreComing")
            .is_some_and(|value| value.as_bool() != Some(false))
            || listing
                .get("continuationMarker")
                .is_some_and(|value| !value.is_null())
            || zones.len() > 32
        {
            return Err("zone scope is incomplete or exceeds bounded probe");
        }
        let primary_photos = zones
            .iter()
            .filter(|zone| {
                zone.pointer("/zoneID/zoneName").and_then(Value::as_str) == Some("PrimarySync")
            })
            .count();
        let shared_photos = zones
            .iter()
            .filter(|zone| {
                zone.pointer("/zoneID/zoneName")
                    .and_then(Value::as_str)
                    .is_some_and(|name| name.starts_with("SharedSync-"))
            })
            .count();
        eprintln!(
            "database={database} primary_photos_zones={primary_photos} shared_photos_zones={shared_photos}"
        );
        let mut token_bearing = 0;
        let mut accepted = 0;
        let mut rejected = 0;
        // Listing anomalies fail this diagnostic conservatively, even if a
        // malformed entry cannot be identified as a Photos zone. They are not
        // interpreted as a production sync failure or proof of missing content.
        for zone in zones {
            let Some((zone_id, token)) = photos_cursor_candidate(zone)? else {
                continue;
            };
            token_bearing += 1;
            let body =
                crate::icloud::photos::queries::build_changes_zone_request(zone_id, Some(token), 1);
            let raw = crate::icloud::photos::session::diagnostic_bytes(
                session.http_client(),
                &format!("{endpoint}/changes/zone"),
                body.to_string(),
                &[("Content-type", "text/plain")],
                1024 * 1024,
            )
            .await
            .map_err(|_error| "read-only changes cursor request failed")?;
            let response: Value =
                serde_json::from_slice(&raw).map_err(|_error| "changes response invalid")?;
            if cursor_response_matches_scope(&response, zone_id) {
                accepted += 1;
            } else {
                rejected += 1;
            }
        }
        accepted_total += accepted;
        eprintln!(
            "database={database} zones={} token_bearing={token_bearing} cursors_accepted={accepted} cursors_rejected={rejected}",
            zones.len()
        );
        if rejected > 0 {
            return Err(
                "Cursor acceptance was not established for every token-bearing Photos zone",
            );
        }
    }
    if accepted_total == 0 {
        return Err("no Photos zone metadata cursor was available for validation");
    }
    eprintln!(
        "photos_cursor_acceptance_only=true snapshot_consistency_proven=false provider_write_endpoints_called=0"
    );
    Ok(())
}

#[test]
fn synthetic_cursor_probe_selects_only_token_bearing_photos_zones() {
    use serde_json::json;
    for name in ["PrimarySync", "SharedSync-synthetic"] {
        let zone = json!({"zoneID":{"zoneName":name,"ownerRecordName":"synthetic-owner"},"syncToken":"synthetic-cursor"});
        let (id, token) = photos_cursor_candidate(&zone).unwrap().unwrap();
        assert_eq!(id, &zone["zoneID"]);
        assert_eq!(token, "synthetic-cursor");
    }
    for zone in [
        json!({"zoneID":{"zoneName":"UnrelatedZone"},"syncToken":"synthetic-cursor"}),
        json!({"zoneID":{"zoneName":"PrimarySync"},"syncToken":"  "}),
        json!({"zoneID":{"zoneName":"PrimarySync"}}),
        json!({"zoneID":{"zoneName":"PrimarySync"},"syncToken":"synthetic-cursor","deleted":true}),
    ] {
        assert!(photos_cursor_candidate(&zone).unwrap().is_none());
    }
    assert!(photos_cursor_candidate(&json!({"syncToken":"synthetic-cursor"})).is_err());
    assert!(photos_cursor_candidate(&json!({"serverErrorCode":"synthetic-error"})).is_err());
}

#[test]
fn synthetic_cursor_probe_requires_exact_returned_scope() {
    use serde_json::json;
    let id = json!({"zoneName":"PrimarySync","ownerRecordName":"synthetic-owner"});
    let good = json!({"zoneID":id,"syncToken":"synthetic-next"});
    assert!(cursor_response_matches_scope(
        &json!({"zones":[good.clone()]}),
        &id
    ));
    // Acceptance is only acceptance: a continuing response is not an EOF proof.
    assert!(cursor_response_matches_scope(
        &json!({"zones":[good.clone()],"moreComing":true}),
        &id
    ));
    for response in [
        json!({}),
        json!({"zones":[]}),
        json!({"zones":[good.clone(),good.clone()]}),
        json!({"zones":[{"zoneID":{"zoneName":"PrimarySync","ownerRecordName":"other-owner"},"syncToken":"synthetic-next"}]}),
        json!({"zones":[{"zoneID":{"zoneName":"UnrelatedZone","ownerRecordName":"synthetic-owner"},"syncToken":"synthetic-next"}]}),
        json!({"zones":[{"zoneID":id,"syncToken":" "}]}),
        json!({"zones":[{"zoneID":id,"syncToken":"synthetic-next","serverErrorCode":"synthetic-error"}]}),
    ] {
        assert!(!cursor_response_matches_scope(&response, &id));
    }
}
