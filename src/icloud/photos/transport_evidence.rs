//! Versioned transport evidence for validated private provider changes pages.
//!
//! The adapter recognizes two direct provider descriptors, bound to the
//! enclosing record's nonblank `recordName` and `recordChangeTag`:
//!
//! - `CPLMaster` `fields.resOriginalRes` `ASSETID`
//! - `CPLAsset` or `CPLMaster` `fields.resComputeStateData` `ASSETID`
//!
//! Each descriptor must carry a numeric `size`, nonblank `fileChecksum`,
//! `referenceChecksum` and `wrappingKey`, and a validated `downloadURL`. It
//! masks only the entire validated `downloadURL` string token. Every other
//! byte, including record type, tag, descriptor fields, unknown fields,
//! optional `receipt`, record order, whitespace and numeric lexemes, is
//! retained exactly. Nested or arbitrary `downloadURL` fields are never
//! eligible.
//!
//! The complete descriptor plus record version is a provider-versioned asset
//! identity. It is never local media proof, verified content hash, ownership,
//! or checkpoint authority.

use anyhow::{Context, Result, bail};
use serde_json::Value;

/// Version of the byte-level masking and eligibility rule implemented here.
pub(crate) const TRANSPORT_EVIDENCE_VERSION: u8 = 3;

/// Deterministic replacement for one eligible URL string token, including its
/// surrounding JSON quotes.
pub(crate) const MASKED_URL_TOKEN: &[u8] = b"\"<redacted-transport-url>\"";

const MAX_JSON_DEPTH: usize = 128;
const PRIVATE_MASTER_RECORD_TYPE: &str = "CPLMaster";
const PRIVATE_ASSET_RECORD_TYPE: &str = "CPLAsset";
const PRIVATE_ORIGINAL_FIELD: &str = "resOriginalRes";
const PRIVATE_COMPUTE_STATE_FIELD: &str = "resComputeStateData";
const PRIVATE_DESCRIPTOR_ASSET_TYPE: &str = "ASSETID";
const DOWNLOAD_URL_FIELD: &str = "downloadURL";

/// Exact masked bytes for a validated page with at least one eligible URL.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TransportEvidence {
    pub(crate) version: u8,
    pub(crate) masked: Vec<u8>,
    pub(crate) eligible_url_tokens: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum PathSegment {
    Key(String),
    Index(usize),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DescriptorField {
    Original,
    ComputeState,
}

/// The exact envelope that produced a candidate path. Eligibility is resolved
/// only against the records of this envelope, never against a lookalike record
/// at the same ordinal in another envelope.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Envelope {
    Changes,
    Lookup,
}

impl DescriptorField {
    fn name(self) -> &'static str {
        match self {
            Self::Original => PRIVATE_ORIGINAL_FIELD,
            Self::ComputeState => PRIVATE_COMPUTE_STATE_FIELD,
        }
    }
}

struct Candidate {
    start: usize,
    end: usize,
    url: String,
    envelope: Envelope,
    record_index: usize,
    field: DescriptorField,
}

/// Provider-owned evidence recomputation for one persisted capture. The
/// adapter proves the private scope and re-validates the exact body metadata
/// and identities through the live page contract before classifying bytes.
///
/// Unsupported scopes are ordinary raw captures. A private scope that is
/// malformed, or a body that no longer matches the caller's page, is invalid.
pub(crate) fn capture_evidence(
    page: &crate::state::db::provider_inbox::ObservedPage,
) -> Result<Option<TransportEvidence>> {
    let scope: Value =
        serde_json::from_str(&page.scope).context("Invalid provider capture scope")?;
    let scope = scope
        .as_object()
        .context("Invalid provider capture scope")?;
    if scope.get("format") != Some(&Value::from(1))
        || scope.get("container").and_then(Value::as_str) != Some("com.apple.photos.cloud")
        || scope.get("environment").and_then(Value::as_str) != Some("production")
    {
        return Ok(None);
    }
    // Coalescing requires a complete private scope proof. A scope that is
    // otherwise capturable but lacks the realm, database, zone name or exact
    // owner stays an ordinary raw observation; it is never rejected merely
    // for being ineligible, and it never grants transport eligibility.
    match scope.get("database").and_then(Value::as_str) {
        Some("private") => {}
        Some(_) | None => return Ok(None),
    }
    if !nonblank_string(scope.get("realm")) {
        return Ok(None);
    }
    let zone = scope.get("zone").and_then(Value::as_object);
    let Some(zone) = zone else {
        return Ok(None);
    };
    if !nonblank_string(zone.get("zoneName")) || !nonblank_string(zone.get("ownerRecordName")) {
        return Ok(None);
    }
    let validated =
        super::album::catalog_observed_page(page.body.clone(), &page.scope, &page.request_cursor)?;
    anyhow::ensure!(
        validated == *page,
        "Provider capture page does not match its validated body"
    );
    classify_private_changes(&page.body)
}

/// Mask eligible transport capability tokens in one complete private changes
/// page.
///
/// The caller must independently prove the private database contract and must
/// validate scope, account, container, environment, zone, request cursor,
/// successor, and `moreComing` binding before using the evidence. There is
/// deliberately no shared or public entry point: those pages stay raw even if
/// their JSON looks similar.
///
/// Returns `None` when no record qualifies, which is the raw identity case.
/// Malformed JSON, duplicate keys, an oversized page, or excessive nesting is
/// rejected before any evidence is produced.
pub(crate) fn classify_private_changes(bytes: &[u8]) -> Result<Option<TransportEvidence>> {
    anyhow::ensure!(
        bytes.len() <= super::inbox::MAX_CHANGES_PAGE_BYTES,
        "Provider changes page exceeded the transport evidence byte limit"
    );
    // Reuse the existing strict parser so duplicate keys and ambiguous pages
    // are rejected before byte-span inspection.
    let value = super::changes_json::parse(bytes)?;

    // This explicit adapter rule is the only provider mapping in this stage.
    // It never consults arbitrary resource dictionaries or rendition lists.
    let records = value
        .get("zones")
        .and_then(Value::as_array)
        .and_then(|zones| zones.first())
        .and_then(|zone| zone.get("records"))
        .and_then(Value::as_array);
    let Some(records) = records else {
        return Ok(None);
    };

    let mut position = 0usize;
    let mut path = Vec::new();
    let mut candidates = Vec::new();
    walk(bytes, &mut position, &value, 0, &mut path, &mut candidates)?;
    // Valid JSON may end with whitespace; every such byte stays in the output.
    skip_whitespace(bytes, &mut position);
    anyhow::ensure!(
        position == bytes.len(),
        "Trailing bytes after provider changes JSON"
    );

    evidence_from_candidates(bytes, records, Envelope::Changes, candidates)
}

/// Mask eligible transport capability tokens in the exact paired current
/// lookup confirmation produced by `album::work::current_asset`.
///
/// The caller must already have authenticated and validated the current
/// confirmation through that same contract. This classifier only recognizes
/// the exact two-record envelope for the requested child and master, with the
/// child's `masterRef` pointing at the master. Any other lookup shape stays
/// raw. It masks only the same complete `ASSETID` descriptor paths as the
/// changes-page classifier and preserves every other byte.
pub(crate) fn classify_lookup_confirmation(
    bytes: &[u8],
    child: &str,
    master: &str,
) -> Result<Option<TransportEvidence>> {
    anyhow::ensure!(
        bytes.len() <= super::inbox::MAX_CHANGES_PAGE_BYTES,
        "Provider lookup confirmation exceeded the transport evidence byte limit"
    );
    if child == master {
        return Ok(None);
    }
    let value = super::changes_json::parse(bytes)?;
    let records = value.get("records").and_then(Value::as_array);
    let Some(records) = records else {
        return Ok(None);
    };
    if records.len() != 2 {
        return Ok(None);
    }
    let Some(child_record) = records
        .iter()
        .find(|record| record.get("recordName").and_then(Value::as_str) == Some(child))
    else {
        return Ok(None);
    };
    let Some(master_record) = records
        .iter()
        .find(|record| record.get("recordName").and_then(Value::as_str) == Some(master))
    else {
        return Ok(None);
    };
    if child_record.get("recordType").and_then(Value::as_str) != Some(PRIVATE_ASSET_RECORD_TYPE)
        || master_record.get("recordType").and_then(Value::as_str)
            != Some(PRIVATE_MASTER_RECORD_TYPE)
        || child_record
            .pointer("/fields/masterRef/value/recordName")
            .and_then(Value::as_str)
            != Some(master)
    {
        return Ok(None);
    }

    let mut position = 0usize;
    let mut path = Vec::new();
    let mut candidates = Vec::new();
    walk(bytes, &mut position, &value, 0, &mut path, &mut candidates)?;
    skip_whitespace(bytes, &mut position);
    anyhow::ensure!(
        position == bytes.len(),
        "Trailing bytes after provider lookup JSON"
    );
    evidence_from_candidates(bytes, records, Envelope::Lookup, candidates)
}

fn evidence_from_candidates(
    bytes: &[u8],
    records: &[Value],
    envelope: Envelope,
    candidates: Vec<Candidate>,
) -> Result<Option<TransportEvidence>> {
    let mut spans = Vec::new();
    for candidate in candidates {
        if candidate.envelope != envelope {
            continue;
        }
        let Some(record) = records.get(candidate.record_index) else {
            continue;
        };
        if !eligible_private_descriptor(record, candidate.field) {
            continue;
        }
        if !eligible_download_url(&candidate.url) {
            continue;
        }
        spans.push((candidate.start, candidate.end));
    }

    if spans.is_empty() {
        return Ok(None);
    }

    let mut masked = Vec::with_capacity(bytes.len());
    let mut cursor = 0usize;
    for (start, end) in &spans {
        masked.extend_from_slice(
            bytes
                .get(cursor..*start)
                .context("Invalid transport evidence span")?,
        );
        masked.extend_from_slice(MASKED_URL_TOKEN);
        cursor = *end;
    }
    masked.extend_from_slice(
        bytes
            .get(cursor..)
            .context("Invalid transport evidence tail")?,
    );

    Ok(Some(TransportEvidence {
        version: TRANSPORT_EVIDENCE_VERSION,
        masked,
        eligible_url_tokens: spans.len(),
    }))
}

fn walk(
    bytes: &[u8],
    position: &mut usize,
    node: &Value,
    depth: usize,
    path: &mut Vec<PathSegment>,
    candidates: &mut Vec<Candidate>,
) -> Result<()> {
    if depth > MAX_JSON_DEPTH {
        bail!("Provider changes JSON exceeded the evidence depth limit");
    }
    skip_whitespace(bytes, position);
    match current(bytes, *position)? {
        b'{' => walk_object(bytes, position, node, depth, path, candidates),
        b'[' => walk_array(bytes, position, node, depth, path, candidates),
        b'"' => {
            let start = *position;
            let (end, url) = scan_string(bytes, start)?;
            if let Some((envelope, record_index, field)) = candidate_record_index(path) {
                candidates.push(Candidate {
                    start,
                    end,
                    url,
                    envelope,
                    record_index,
                    field,
                });
            }
            *position = end;
            Ok(())
        }
        b't' => expect_literal(bytes, position, b"true"),
        b'f' => expect_literal(bytes, position, b"false"),
        b'n' => expect_literal(bytes, position, b"null"),
        b'-' | b'0'..=b'9' => scan_number(bytes, position),
        _ => bail!("Invalid JSON value in provider changes page"),
    }
}

fn walk_object(
    bytes: &[u8],
    position: &mut usize,
    node: &Value,
    depth: usize,
    path: &mut Vec<PathSegment>,
    candidates: &mut Vec<Candidate>,
) -> Result<()> {
    *position += 1;
    skip_whitespace(bytes, position);
    if current(bytes, *position)? == b'}' {
        *position += 1;
        return Ok(());
    }
    loop {
        let (end, key) = scan_string(bytes, *position)?;
        *position = end;
        skip_whitespace(bytes, position);
        expect_byte(bytes, position, b':')?;
        skip_whitespace(bytes, position);
        let child = node
            .get(key.as_str())
            .context("JSON structure mismatch in provider changes page")?;
        path.push(PathSegment::Key(key));
        walk(bytes, position, child, depth + 1, path, candidates)?;
        path.pop();
        skip_whitespace(bytes, position);
        match current(bytes, *position)? {
            b',' => {
                *position += 1;
                skip_whitespace(bytes, position);
            }
            b'}' => {
                *position += 1;
                return Ok(());
            }
            _ => bail!("Invalid JSON object in provider changes page"),
        }
    }
}

fn walk_array(
    bytes: &[u8],
    position: &mut usize,
    node: &Value,
    depth: usize,
    path: &mut Vec<PathSegment>,
    candidates: &mut Vec<Candidate>,
) -> Result<()> {
    *position += 1;
    skip_whitespace(bytes, position);
    if current(bytes, *position)? == b']' {
        *position += 1;
        return Ok(());
    }
    let mut index = 0usize;
    loop {
        let child = node
            .get(index)
            .context("JSON structure mismatch in provider changes page")?;
        path.push(PathSegment::Index(index));
        walk(bytes, position, child, depth + 1, path, candidates)?;
        path.pop();
        skip_whitespace(bytes, position);
        match current(bytes, *position)? {
            b',' => {
                *position += 1;
                skip_whitespace(bytes, position);
                index = index
                    .checked_add(1)
                    .context("Provider changes JSON array overflow")?;
            }
            b']' => {
                *position += 1;
                return Ok(());
            }
            _ => bail!("Invalid JSON array in provider changes page"),
        }
    }
}

fn candidate_record_index(path: &[PathSegment]) -> Option<(Envelope, usize, DescriptorField)> {
    // Changes envelope: `zones[0].records[n].fields.<field>.value.downloadURL`.
    // Current lookup envelope: `records[n].fields.<field>.value.downloadURL`.
    let (envelope, record_index, fields, field, value, download_url) = match path {
        [
            PathSegment::Key(zones),
            PathSegment::Index(0),
            PathSegment::Key(records),
            PathSegment::Index(record_index),
            PathSegment::Key(fields),
            PathSegment::Key(field),
            PathSegment::Key(value),
            PathSegment::Key(download_url),
        ] if zones.as_str() == "zones" && records.as_str() == "records" => (
            Envelope::Changes,
            record_index,
            fields,
            field,
            value,
            download_url,
        ),
        [
            PathSegment::Key(records),
            PathSegment::Index(record_index),
            PathSegment::Key(fields),
            PathSegment::Key(field),
            PathSegment::Key(value),
            PathSegment::Key(download_url),
        ] if records.as_str() == "records" => (
            Envelope::Lookup,
            record_index,
            fields,
            field,
            value,
            download_url,
        ),
        _ => return None,
    };
    if fields.as_str() != "fields"
        || value.as_str() != "value"
        || download_url.as_str() != DOWNLOAD_URL_FIELD
    {
        return None;
    }
    let field = match field.as_str() {
        PRIVATE_ORIGINAL_FIELD => DescriptorField::Original,
        PRIVATE_COMPUTE_STATE_FIELD => DescriptorField::ComputeState,
        _ => return None,
    };
    Some((envelope, *record_index, field))
}

/// Require the complete fetched private descriptor for the exact direct field
/// path that produced this candidate.
///
/// Unknown extra descriptor or record fields are allowed and preserved. Only
/// the listed fields are required; `receipt` is optional because Apple returns
/// it for saving, not for a fetched private download descriptor. The original
/// descriptor remains `CPLMaster` only. The approved compute-state extension
/// accepts only the two known CloudKit record types and never walks another
/// field name, nested dictionary or rendition list.
fn eligible_private_descriptor(record: &Value, field: DescriptorField) -> bool {
    let record_type = record.get("recordType").and_then(Value::as_str);
    let record_type_allowed = match field {
        DescriptorField::Original => record_type == Some(PRIVATE_MASTER_RECORD_TYPE),
        DescriptorField::ComputeState => matches!(
            record_type,
            Some(PRIVATE_ASSET_RECORD_TYPE | PRIVATE_MASTER_RECORD_TYPE)
        ),
    };
    if !record_type_allowed {
        return false;
    }
    if !nonblank_string(record.get("recordName")) || !nonblank_string(record.get("recordChangeTag"))
    {
        return false;
    }
    let Some(resource) = record
        .get("fields")
        .and_then(Value::as_object)
        .and_then(|fields| fields.get(field.name()))
    else {
        return false;
    };
    if resource.get("type").and_then(Value::as_str) != Some(PRIVATE_DESCRIPTOR_ASSET_TYPE) {
        return false;
    }
    let Some(value) = resource.get("value").and_then(Value::as_object) else {
        return false;
    };
    value.get("size").and_then(Value::as_u64).is_some()
        && nonblank_string(value.get("fileChecksum"))
        && nonblank_string(value.get("referenceChecksum"))
        && nonblank_string(value.get("wrappingKey"))
}

fn nonblank_string(value: Option<&Value>) -> bool {
    value
        .and_then(Value::as_str)
        .is_some_and(|text| !text.trim().is_empty())
}

fn eligible_download_url(raw: &str) -> bool {
    url::Url::parse(raw).is_ok_and(|url| url.scheme() == "https")
        && super::asset::validate_download_url(raw).is_ok()
}

fn current(bytes: &[u8], position: usize) -> Result<u8> {
    bytes
        .get(position)
        .copied()
        .context("Unexpected end of provider changes JSON")
}

fn skip_whitespace(bytes: &[u8], position: &mut usize) {
    while matches!(
        bytes.get(*position).copied(),
        Some(b' ' | b'\t' | b'\n' | b'\r')
    ) {
        *position += 1;
    }
}

fn expect_byte(bytes: &[u8], position: &mut usize, expected: u8) -> Result<()> {
    if bytes.get(*position).copied() != Some(expected) {
        bail!("Invalid JSON byte in provider changes page");
    }
    *position += 1;
    Ok(())
}

fn expect_literal(bytes: &[u8], position: &mut usize, literal: &[u8]) -> Result<()> {
    let end = position
        .checked_add(literal.len())
        .context("Provider changes JSON position overflow")?;
    if bytes.get(*position..end) != Some(literal) {
        bail!("Invalid JSON literal in provider changes page");
    }
    *position = end;
    Ok(())
}

fn scan_string(bytes: &[u8], start: usize) -> Result<(usize, String)> {
    if bytes.get(start).copied() != Some(b'"') {
        bail!("Invalid JSON string in provider changes page");
    }
    let mut index = start + 1;
    loop {
        match bytes.get(index).copied() {
            Some(b'"') => {
                let end = index + 1;
                let token = bytes
                    .get(start..end)
                    .context("Invalid JSON string span in provider changes page")?;
                let value = serde_json::from_slice::<String>(token)
                    .context("Invalid JSON string token in provider changes page")?;
                return Ok((end, value));
            }
            Some(b'\\') => index += 2,
            Some(_) => index += 1,
            None => bail!("Unterminated JSON string in provider changes page"),
        }
    }
}

fn scan_number(bytes: &[u8], position: &mut usize) -> Result<()> {
    let start = *position;
    while matches!(
        bytes.get(*position).copied(),
        Some(b'-' | b'+' | b'.' | b'e' | b'E' | b'0'..=b'9')
    ) {
        *position += 1;
    }
    if *position == start {
        bail!("Invalid JSON number in provider changes page");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    const VALID_URL: &str = "https://p01.icloud-content.com/asset-1?x=1";

    const VALID_PAGE: &str = r#"{"zones":[{"zoneID":{"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"},"syncToken":"successor","moreComing":false,"records":[{"recordName":"master-1","recordType":"CPLMaster","recordChangeTag":"tag-1","fields":{"resOriginalRes":{"type":"ASSETID","value":{"size":1000,"downloadURL":"__URL__","fileChecksum":"checksum-A","referenceChecksum":"reference-A","wrappingKey":"wrapping-A","receipt":"receipt-A","unknown":{"value":1.00}}},"filenameEnc":{"value":"kept"}},"unknownRecordField":{"value":"kept"}}]}]}"#;

    const TAG: &str = r#""recordChangeTag":"tag-1","#;

    const COMPUTE_PAGE: &str = r#"{"zones":[{"zoneID":{"zoneName":"PrimarySync"},"syncToken":"successor","moreComing":false,"records":[{"recordName":"asset-1","recordType":"CPLAsset","recordChangeTag":"asset-tag-1","fields":{"resComputeStateData":{"type":"ASSETID","value":{"size":2048,"downloadURL":"__URL__","fileChecksum":"compute-checksum","referenceChecksum":"compute-reference","wrappingKey":"compute-wrapping","receipt":"compute-receipt","unknown":{"value":1.00}}},"filenameEnc":{"value":"kept"}},"unknownRecordField":{"value":"kept"}}]}]}"#;

    fn valid_page() -> String {
        VALID_PAGE.replace("__URL__", VALID_URL)
    }

    fn compute_page(url: &str) -> Vec<u8> {
        COMPUTE_PAGE.replace("__URL__", url).into_bytes()
    }

    fn with_compute(from: &str, to: &str) -> Vec<u8> {
        let changed = COMPUTE_PAGE.replace(from, to);
        assert_ne!(changed, COMPUTE_PAGE, "{from}");
        changed.replace("__URL__", VALID_URL).into_bytes()
    }

    fn mixed_page(original_url: &str, compute_url: &str) -> Vec<u8> {
        format!(
            r#"{{"zones":[{{"zoneID":{{"zoneName":"PrimarySync"}},"syncToken":"successor","moreComing":false,"records":[{{"recordName":"master-1","recordType":"CPLMaster","recordChangeTag":"tag-1","fields":{{"resOriginalRes":{{"type":"ASSETID","value":{{"size":1000,"downloadURL":"{original_url}","fileChecksum":"checksum-A","referenceChecksum":"reference-A","wrappingKey":"wrapping-A"}}}}}}}},{{"recordName":"asset-1","recordType":"CPLAsset","recordChangeTag":"asset-tag-1","fields":{{"resComputeStateData":{{"type":"ASSETID","value":{{"size":2048,"downloadURL":"{compute_url}","fileChecksum":"compute-checksum","referenceChecksum":"compute-reference","wrappingKey":"compute-wrapping"}}}}}}}}]}}]}}"#
        )
        .into_bytes()
    }

    fn page_with_url(url: &str) -> Vec<u8> {
        VALID_PAGE.replace("__URL__", url).into_bytes()
    }

    fn classify_ok(body: &[u8]) -> super::TransportEvidence {
        super::classify_private_changes(body).unwrap().unwrap()
    }

    fn assert_not_eligible(body: &[u8]) {
        assert!(super::classify_private_changes(body).unwrap().is_none());
    }

    fn without(fragment: &str) -> Vec<u8> {
        let changed = valid_page().replace(fragment, "");
        assert_ne!(changed, valid_page(), "{fragment}");
        changed.into_bytes()
    }

    fn with(from: &str, to: &str) -> Vec<u8> {
        let changed = valid_page().replace(from, to);
        assert_ne!(changed, valid_page(), "{from}");
        changed.into_bytes()
    }

    fn masked_text(evidence: &super::TransportEvidence) -> String {
        String::from_utf8(evidence.masked.clone()).unwrap()
    }

    #[test]
    fn minimum_supported_original_is_masked_exactly() {
        let raw = valid_page();
        let evidence = classify_ok(raw.as_bytes());
        assert_eq!(evidence.version, super::TRANSPORT_EVIDENCE_VERSION);
        assert_eq!(evidence.eligible_url_tokens, 1);

        let expected = raw.replace(
            &format!("\"{VALID_URL}\""),
            std::str::from_utf8(super::MASKED_URL_TOKEN).unwrap(),
        );
        assert_eq!(evidence.masked, expected.as_bytes());
        assert_eq!(
            evidence.masked.len(),
            raw.len() - VALID_URL.len() - 2 + super::MASKED_URL_TOKEN.len()
        );

        let masked = masked_text(&evidence);
        assert!(!masked.contains(VALID_URL));
        assert!(!masked.contains("icloud-content.com"));
        for kept in [
            r#""recordName":"master-1""#,
            r#""recordType":"CPLMaster""#,
            r#""recordChangeTag":"tag-1""#,
            r#""type":"ASSETID""#,
            r#""size":1000"#,
            r#""fileChecksum":"checksum-A""#,
            r#""referenceChecksum":"reference-A""#,
            r#""wrappingKey":"wrapping-A""#,
            r#""receipt":"receipt-A""#,
            r#""unknown":{"value":1.00}"#,
            r#""filenameEnc":{"value":"kept"}"#,
            r#""unknownRecordField":{"value":"kept"}"#,
        ] {
            assert!(masked.contains(kept), "{kept}");
        }
    }

    #[test]
    fn valid_url_rotation_masks_to_one_representation() {
        let base = classify_ok(&page_with_url(VALID_URL));
        for url in [
            "https://p99.icloud-content.com/asset-1",
            "https://p01.icloud-content.com/a/much/longer/path?token=abc&x=1&y=2",
            "https://p02.cdn-apple.com/asset-1?long=abcdefghijklmnopqrstuvwxyz0123456789",
            "https://p01.icloud-content.com.cn/asset-1?x=1",
        ] {
            let rotated = classify_ok(&page_with_url(url));
            assert_eq!(rotated.masked, base.masked, "{url}");
            assert_eq!(rotated.eligible_url_tokens, 1);
            let masked = masked_text(&rotated);
            assert!(!masked.contains(url), "{url}");
            assert!(!masked.contains("https://"), "{url}");
        }
    }

    #[test]
    fn missing_empty_or_changing_tag_is_not_eligible() {
        for tag in [
            "",
            r#""recordChangeTag":"","#,
            r#""recordChangeTag":"   ","#,
        ] {
            let body = valid_page().replace(TAG, tag);
            assert_not_eligible(body.as_bytes());
        }

        let changed = classify_ok(&with(r#""tag-1""#, r#""tag-2""#));
        let base = classify_ok(valid_page().as_bytes());
        assert_ne!(changed.masked, base.masked);
        assert!(masked_text(&changed).contains(r#""recordChangeTag":"tag-2""#));
    }

    #[test]
    fn missing_required_private_fields_are_not_eligible() {
        for fragment in [
            r#""recordName":"master-1","#,
            r#""recordType":"CPLMaster","#,
            TAG,
            r#""type":"ASSETID","#,
            r#""size":1000,"#,
            r#""fileChecksum":"checksum-A","#,
            r#""referenceChecksum":"reference-A","#,
            r#""wrappingKey":"wrapping-A","#,
            r#""downloadURL":"https://p01.icloud-content.com/asset-1?x=1","#,
        ] {
            assert_not_eligible(&without(fragment));
        }

        for (from, to) in [
            (r#""recordName":"master-1""#, r#""recordName":"   ""#),
            (TAG, r#""recordChangeTag":"   ","#),
            (r#""type":"ASSETID""#, r#""type":"OTHER""#),
            (r#""size":1000"#, r#""size":"1000""#),
            (r#""size":1000"#, r#""size":1000.0"#),
            (r#""size":1000"#, r#""size":-1"#),
            (r#""fileChecksum":"checksum-A""#, r#""fileChecksum":"   ""#),
            (r#""fileChecksum":"checksum-A""#, r#""fileChecksum":7"#),
            (
                r#""referenceChecksum":"reference-A""#,
                r#""referenceChecksum":"   ""#,
            ),
            (
                r#""referenceChecksum":"reference-A""#,
                r#""referenceChecksum":null"#,
            ),
            (r#""wrappingKey":"wrapping-A""#, r#""wrappingKey":"   ""#),
            (r#""wrappingKey":"wrapping-A""#, r#""wrappingKey":false"#),
            (r#""recordName":"master-1""#, r#""recordName":7"#),
            (TAG, r#""recordChangeTag":7,"#),
        ] {
            assert_not_eligible(&with(from, to));
        }
    }

    #[test]
    fn checksum_alone_does_not_qualify() {
        let body = valid_page()
            .replace(r#""referenceChecksum":"reference-A","#, "")
            .replace(r#""wrappingKey":"wrapping-A","#, "")
            .replace(r#""type":"ASSETID","#, "")
            .replace(TAG, "");
        assert_not_eligible(body.as_bytes());
    }

    #[test]
    fn lookalike_type_record_and_path_are_not_eligible() {
        assert_not_eligible(&with(
            r#""recordType":"CPLMaster""#,
            r#""recordType":"CPLAsset""#,
        ));
        assert_not_eligible(&with(r#""resOriginalRes""#, r#""resOriginalAltRes""#));

        let raw = valid_page();
        let mut zones = raw.replacen(
            r#"{"zones":[{""#,
            r#"{"zones":[{"zoneID":{"zoneName":"PrimarySync"},"records":[]},{""#,
            1,
        );
        if zones == raw {
            zones = raw.replacen(r#"{"zones":[{""#, r#"{"zones":[{"records":[]},{""#, 1);
        }
        assert_ne!(zones, raw);
        assert_not_eligible(zones.as_bytes());

        let top_level_records = format!(
            r#"{{"records":[{records}]}}"#,
            records = raw
                .split(r#""records":["#)
                .nth(1)
                .unwrap()
                .rsplitn(2, r#"]}]}"#)
                .last()
                .unwrap()
        );
        assert_not_eligible(top_level_records.as_bytes());

        let nested = valid_page().replace(
            r#""downloadURL":"https://p01.icloud-content.com/asset-1?x=1","#,
            r#""nested":{"downloadURL":"https://p01.icloud-content.com/asset-1?x=1"},"#,
        );
        assert_not_eligible(nested.as_bytes());
    }

    #[test]
    fn compute_state_url_rotation_masks_to_one_representation() {
        let base = classify_ok(&compute_page(VALID_URL));
        assert_eq!(base.version, super::TRANSPORT_EVIDENCE_VERSION);
        assert_eq!(base.eligible_url_tokens, 1);
        let masked = masked_text(&base);
        assert!(masked.contains(r#""recordType":"CPLAsset""#));
        assert!(masked.contains(r#""resComputeStateData":{"type":"ASSETID""#));
        assert!(masked.contains(r#""fileChecksum":"compute-checksum""#));
        assert!(masked.contains(r#""referenceChecksum":"compute-reference""#));
        assert!(masked.contains(r#""wrappingKey":"compute-wrapping""#));
        assert!(masked.contains(r#""receipt":"compute-receipt""#));
        assert!(masked.contains(r#""unknown":{"value":1.00}"#));
        assert!(!masked.contains(VALID_URL));

        for url in [
            "https://p99.icloud-content.com/compute",
            "https://p01.icloud-content.com/a/much/longer/path?token=abc&x=1&y=2",
            "https://p02.cdn-apple.com/compute?long=abcdefghijklmnopqrstuvwxyz0123456789",
        ] {
            let rotated = classify_ok(&compute_page(url));
            assert_eq!(rotated.masked, base.masked, "{url}");
            assert_eq!(rotated.eligible_url_tokens, 1);
            assert!(!masked_text(&rotated).contains(url), "{url}");
        }

        let master_compute = COMPUTE_PAGE
            .replace(r#""recordType":"CPLAsset""#, r#""recordType":"CPLMaster""#)
            .replace("__URL__", VALID_URL);
        assert_eq!(
            classify_ok(master_compute.as_bytes()).eligible_url_tokens,
            1
        );
    }

    #[test]
    fn compute_state_missing_type_tag_or_tuple_is_not_eligible() {
        for fragment in [
            r#""recordName":"asset-1","#,
            r#""recordType":"CPLAsset","#,
            r#""recordChangeTag":"asset-tag-1","#,
            r#""type":"ASSETID","#,
            r#""size":2048,"#,
            r#""fileChecksum":"compute-checksum","#,
            r#""referenceChecksum":"compute-reference","#,
            r#""wrappingKey":"compute-wrapping","#,
            r#""downloadURL":"__URL__","#,
        ] {
            let changed = COMPUTE_PAGE
                .replace(fragment, "")
                .replace("__URL__", VALID_URL);
            assert_ne!(
                changed,
                COMPUTE_PAGE.replace("__URL__", VALID_URL),
                "{fragment}"
            );
            assert_not_eligible(changed.as_bytes());
        }

        for (from, to) in [
            (r#""recordName":"asset-1""#, r#""recordName":"   ""#),
            (r#""recordType":"CPLAsset""#, r#""recordType":"CPLUnknown""#),
            (r#""recordType":"CPLAsset""#, r#""recordType":7"#),
            (
                r#""recordChangeTag":"asset-tag-1""#,
                r#""recordChangeTag":"""#,
            ),
            (
                r#""recordChangeTag":"asset-tag-1""#,
                r#""recordChangeTag":"   ""#,
            ),
            (
                r#""recordChangeTag":"asset-tag-1""#,
                r#""recordChangeTag":7"#,
            ),
            (r#""type":"ASSETID""#, r#""type":"OTHER""#),
            (r#""size":2048"#, r#""size":"2048""#),
            (r#""size":2048"#, r#""size":2048.0"#),
            (
                r#""fileChecksum":"compute-checksum""#,
                r#""fileChecksum":"   ""#,
            ),
            (
                r#""referenceChecksum":"compute-reference""#,
                r#""referenceChecksum":null"#,
            ),
            (
                r#""wrappingKey":"compute-wrapping""#,
                r#""wrappingKey":false"#,
            ),
        ] {
            assert_not_eligible(&with_compute(from, to));
        }
    }

    #[test]
    fn compute_state_byte_changes_and_continuation_remain_distinct() {
        let base = classify_ok(&compute_page(VALID_URL));
        for (from, to) in [
            (
                r#""fileChecksum":"compute-checksum""#,
                r#""fileChecksum":"compute-checksum-B""#,
            ),
            (
                r#""referenceChecksum":"compute-reference""#,
                r#""referenceChecksum":"compute-reference-B""#,
            ),
            (
                r#""wrappingKey":"compute-wrapping""#,
                r#""wrappingKey":"compute-wrapping-B""#,
            ),
            (r#""size":2048"#, r#""size":2049"#),
            (
                r#""receipt":"compute-receipt""#,
                r#""receipt":"compute-receipt-B""#,
            ),
            (r#""unknown":{"value":1.00}"#, r#""unknown":{"value":2.00}"#),
            (r#""recordName":"asset-1""#, r#""recordName":"asset-2""#),
            (
                r#""recordChangeTag":"asset-tag-1""#,
                r#""recordChangeTag":"asset-tag-2""#,
            ),
            (r#""syncToken":"successor""#, r#""syncToken":"successor-2""#),
            (r#""moreComing":false"#, r#""moreComing":true"#),
        ] {
            let changed = classify_ok(&with_compute(from, to));
            assert_ne!(changed.masked, base.masked, "{from}");
        }
    }

    #[test]
    fn mixed_original_and_compute_url_rotation_is_equal() {
        let base = classify_ok(&mixed_page(
            VALID_URL,
            "https://p01.icloud-content.com/compute?token=0",
        ));
        assert_eq!(base.eligible_url_tokens, 2);
        assert!(!masked_text(&base).contains("https://"));

        let rotated = classify_ok(&mixed_page(
            "https://p99.icloud-content.com/original?token=1",
            "https://p02.cdn-apple.com/compute?token=2",
        ));
        assert_eq!(rotated.masked, base.masked);
        assert_eq!(rotated.eligible_url_tokens, 2);

        let changed_compute = String::from_utf8(mixed_page(
            VALID_URL,
            "https://p01.icloud-content.com/compute?token=0",
        ))
        .unwrap()
        .replace(
            r#""wrappingKey":"compute-wrapping""#,
            r#""wrappingKey":"compute-wrapping-B""#,
        );
        let changed = classify_ok(changed_compute.as_bytes());
        assert_ne!(changed.masked, base.masked);
        assert_eq!(changed.eligible_url_tokens, 2);

        let unsupported_compute = String::from_utf8(mixed_page(
            VALID_URL,
            "https://p01.icloud-content.com/compute?token=0",
        ))
        .unwrap()
        .replace(
            r#""resComputeStateData":{"type":"ASSETID""#,
            r#""resComputeStateData":{"type":"OTHER""#,
        );
        let unsupported = classify_ok(unsupported_compute.as_bytes());
        assert_ne!(unsupported.masked, base.masked);
        assert_eq!(unsupported.eligible_url_tokens, 1);
        assert!(
            masked_text(&unsupported).contains("https://p01.icloud-content.com/compute?token=0")
        );
    }

    #[test]
    fn deep_or_nested_compute_lookalikes_stay_raw() {
        let nested = r#"{"zones":[{"zoneID":{"zoneName":"PrimarySync"},"syncToken":"successor","moreComing":false,"records":[{"recordName":"asset-1","recordType":"CPLAsset","recordChangeTag":"asset-tag-1","fields":{"resComputeStateData":{"type":"ASSETID","value":{"size":1,"fileChecksum":"c","referenceChecksum":"r","wrappingKey":"w","downloadURL":{"nested":"https://p01.icloud-content.com/asset-1"}}}}}]}]}"#;
        assert_not_eligible(nested.as_bytes());

        let deep = r#"{"zones":[{"zoneID":{"zoneName":"PrimarySync"},"syncToken":"successor","moreComing":false,"records":[{"recordName":"asset-1","recordType":"CPLAsset","recordChangeTag":"asset-tag-1","fields":{"resComputeStateData":{"type":"ASSETID","value":{"size":1,"fileChecksum":"c","referenceChecksum":"r","wrappingKey":"w","nested":{"downloadURL":"https://p01.icloud-content.com/asset-1"}}}}}]}]}"#;
        assert_not_eligible(deep.as_bytes());

        let unrelated_field = r#"{"zones":[{"zoneID":{"zoneName":"PrimarySync"},"syncToken":"successor","moreComing":false,"records":[{"recordName":"asset-1","recordType":"CPLAsset","recordChangeTag":"asset-tag-1","fields":{"resComputeStateDataAlt":{"type":"ASSETID","value":{"size":1,"downloadURL":"https://p01.icloud-content.com/asset-1","fileChecksum":"c","referenceChecksum":"r","wrappingKey":"w"}}}}]}]}"#;
        assert_not_eligible(unrelated_field.as_bytes());
    }

    fn lookup_page(child_url: &str, master_url: &str) -> Vec<u8> {
        format!(
            r#"{{"records":[{{"recordName":"asset-1","recordType":"CPLAsset","recordChangeTag":"asset-tag-1","fields":{{"masterRef":{{"value":{{"recordName":"master-1"}},"type":"REFERENCE"}},"resComputeStateData":{{"type":"ASSETID","value":{{"size":2048,"downloadURL":"{child_url}","fileChecksum":"compute-checksum","referenceChecksum":"compute-reference","wrappingKey":"compute-wrapping","receipt":"compute-receipt"}}}}}}}},{{"recordName":"master-1","recordType":"CPLMaster","recordChangeTag":"tag-1","fields":{{"resOriginalRes":{{"type":"ASSETID","value":{{"size":1000,"downloadURL":"{master_url}","fileChecksum":"checksum-A","referenceChecksum":"reference-A","wrappingKey":"wrapping-A","unknown":{{"value":1.00}}}}}},"unknownRecordField":{{"value":"kept"}}}}}}],"futureResponse":{{"precision":"1.2300e+30"}}}}"#
        )
        .into_bytes()
    }

    #[test]
    fn lookup_confirmation_url_rotation_masks_to_one_representation() {
        let base = super::classify_lookup_confirmation(
            &lookup_page(VALID_URL, VALID_URL),
            "asset-1",
            "master-1",
        )
        .unwrap()
        .unwrap();
        assert_eq!(base.version, super::TRANSPORT_EVIDENCE_VERSION);
        assert_eq!(base.eligible_url_tokens, 2);
        let masked = masked_text(&base);
        assert!(!masked.contains(VALID_URL));
        assert!(masked.contains(r#""futureResponse":{"precision":"1.2300e+30"}"#));
        assert!(masked.contains(r#""unknownRecordField":{"value":"kept"}"#));
        assert!(masked.contains(r#""unknown":{"value":1.00}"#));

        let rotated = super::classify_lookup_confirmation(
            &lookup_page(
                "https://p99.icloud-content.com/compute?token=1",
                "https://p02.cdn-apple.com/original?token=2",
            ),
            "asset-1",
            "master-1",
        )
        .unwrap()
        .unwrap();
        assert_eq!(rotated.masked, base.masked);
        assert_eq!(rotated.eligible_url_tokens, 2);
    }

    #[test]
    fn lookup_confirmation_requires_the_exact_paired_envelope() {
        let body = lookup_page(VALID_URL, VALID_URL);
        for (child, master) in [("asset-1", "other-master"), ("other-asset", "master-1")] {
            assert!(
                super::classify_lookup_confirmation(&body, child, master)
                    .unwrap()
                    .is_none(),
                "{child}/{master}"
            );
        }

        let extra_record = String::from_utf8(body.clone())
            .unwrap()
            .replace(
                r#"{"recordName":"master-1","recordType":"CPLMaster""#,
                r#"{"recordName":"extra","recordType":"CPLMaster","recordChangeTag":"t","fields":{}},{"recordName":"master-1","recordType":"CPLMaster""#,
            )
            .into_bytes();
        assert!(
            super::classify_lookup_confirmation(&extra_record, "asset-1", "master-1")
                .unwrap()
                .is_none()
        );

        let wrong_ref = String::from_utf8(body.clone())
            .unwrap()
            .replace(
                r#""masterRef":{"value":{"recordName":"master-1"}"#,
                r#""masterRef":{"value":{"recordName":"other-master"}"#,
            )
            .into_bytes();
        assert!(
            super::classify_lookup_confirmation(&wrong_ref, "asset-1", "master-1")
                .unwrap()
                .is_none()
        );

        let text = String::from_utf8(body.clone()).unwrap();
        for changed in [
            text.replace(r#""recordType":"CPLAsset""#, r#""recordType":"CPLUnknown""#),
            text.replace(r#""type":"ASSETID""#, r#""type":"OTHER""#),
            text.replace(
                r#""referenceChecksum":"reference-A""#,
                r#""referenceChecksum":"   ""#,
            )
            .replace(
                r#""referenceChecksum":"compute-reference""#,
                r#""referenceChecksum":"   ""#,
            ),
            text.replace(r#""wrappingKey":"wrapping-A""#, r#""wrappingKey":null"#)
                .replace(
                    r#""wrappingKey":"compute-wrapping""#,
                    r#""wrappingKey":null"#,
                ),
        ] {
            assert_ne!(changed, text);
            assert!(
                super::classify_lookup_confirmation(changed.as_bytes(), "asset-1", "master-1")
                    .unwrap()
                    .is_none()
            );
        }
    }

    #[test]
    fn lookup_confirmation_non_url_changes_remain_distinct() {
        let base = super::classify_lookup_confirmation(
            &lookup_page(VALID_URL, VALID_URL),
            "asset-1",
            "master-1",
        )
        .unwrap()
        .unwrap();
        for (from, to) in [
            (
                r#""fileChecksum":"checksum-A""#,
                r#""fileChecksum":"checksum-B""#,
            ),
            (
                r#""recordChangeTag":"tag-1""#,
                r#""recordChangeTag":"tag-2""#,
            ),
            (r#""unknown":{"value":1.00}"#, r#""unknown":{"value":2.00}"#),
            (
                r#""unknownRecordField":{"value":"kept"}"#,
                r#""unknownRecordField":{"value":"changed"}"#,
            ),
            (r#""size":1000"#, r#""size":1001"#),
        ] {
            let changed = String::from_utf8(lookup_page(VALID_URL, VALID_URL))
                .unwrap()
                .replace(from, to);
            let evidence =
                super::classify_lookup_confirmation(changed.as_bytes(), "asset-1", "master-1")
                    .unwrap()
                    .unwrap();
            assert_ne!(evidence.masked, base.masked, "{from}");
        }
    }

    #[test]
    fn incomplete_private_scope_proof_is_raw_not_rejected() {
        use crate::state::db::provider_inbox::ObservedPage;

        let complete = serde_json::json!({
            "format": 1, "realm": "com", "container": "com.apple.photos.cloud",
            "environment": "production", "database": "private",
            "zone": {"zoneName": "PrimarySync", "ownerRecordName": "_defaultOwner"}
        })
        .to_string();
        let page =
            super::super::catalog_observed_page(valid_page().into_bytes(), &complete, "saved")
                .unwrap();
        assert!(
            super::capture_evidence(&page).unwrap().is_some(),
            "complete private scope must classify"
        );

        let ownerless = serde_json::json!({
            "format": 1, "realm": "com", "container": "com.apple.photos.cloud",
            "environment": "production", "database": "private",
            "zone": {"zoneName": "PrimarySync"}
        })
        .to_string();
        let ownerless_body = valid_page().replace(r#","ownerRecordName":"_defaultOwner""#, "");
        let page =
            super::super::catalog_observed_page(ownerless_body.into_bytes(), &ownerless, "saved")
                .unwrap();
        assert!(super::capture_evidence(&page).unwrap().is_none());

        for scope in [
            serde_json::json!({
                "format": 1, "realm": "", "container": "com.apple.photos.cloud",
                "environment": "production", "database": "private",
                "zone": {"zoneName": "PrimarySync", "ownerRecordName": "_defaultOwner"}
            }),
            serde_json::json!({
                "format": 1, "realm": "com", "container": "com.apple.photos.cloud",
                "environment": "production",
                "zone": {"zoneName": "PrimarySync", "ownerRecordName": "_defaultOwner"}
            }),
            serde_json::json!({
                "format": 1, "realm": "com", "container": "com.apple.photos.cloud",
                "environment": "production", "database": "private",
                "zone": {"ownerRecordName": "_defaultOwner"}
            }),
            serde_json::json!({
                "format": 1, "realm": "com", "container": "com.apple.photos.cloud",
                "environment": "production", "database": "private"
            }),
        ] {
            let page = ObservedPage {
                scope: scope.to_string(),
                request_cursor: "saved".to_owned(),
                successor: "next".to_owned(),
                more_coming: false,
                body: valid_page().into_bytes(),
                identities: Vec::new(),
            };
            assert!(super::capture_evidence(&page).unwrap().is_none(), "{scope}");
        }
    }

    #[test]
    fn descriptor_and_unknown_byte_changes_remain_distinct() {
        let base = classify_ok(valid_page().as_bytes());
        for (from, to) in [
            (
                r#""wrappingKey":"wrapping-A""#,
                r#""wrappingKey":"wrapping-B""#,
            ),
            (
                r#""referenceChecksum":"reference-A""#,
                r#""referenceChecksum":"reference-B""#,
            ),
            (
                r#""fileChecksum":"checksum-A""#,
                r#""fileChecksum":"checksum-B""#,
            ),
            (r#""size":1000"#, r#""size":1001"#),
            (r#""receipt":"receipt-A""#, r#""receipt":"receipt-B""#),
            (r#""unknown":{"value":1.00}"#, r#""unknown":{"value":2.00}"#),
            (r#""unknown":{"value":1.00}"#, r#""unknown":{"value":1.0}"#),
            (
                r#""unknownRecordField":{"value":"kept"}"#,
                r#""unknownRecordField":{"value":"changed"}"#,
            ),
            (r#""recordName":"master-1""#, r#""recordName":"master-2""#),
        ] {
            let changed = classify_ok(&with(from, to));
            assert_ne!(changed.masked, base.masked, "{from}");
        }

        let without_receipt = classify_ok(&without(r#","receipt":"receipt-A""#));
        assert_ne!(without_receipt.masked, base.masked);
        assert_eq!(without_receipt.eligible_url_tokens, 1);
    }

    #[test]
    fn duplicate_keys_are_rejected() {
        let raw = valid_page();
        let cases = [
            (
                format!(r#""downloadURL":"{VALID_URL}","#),
                format!(
                    r#""downloadURL":"{VALID_URL}","downloadURL":"https://p01.icloud-content.com/other","#
                ),
            ),
            (
                TAG.to_string(),
                format!(r#"{TAG}"recordChangeTag":"tag-2","#),
            ),
            (
                r#""type":"ASSETID","#.to_string(),
                r#""type":"ASSETID","type":"OTHER","#.to_string(),
            ),
            (
                r#""size":1000,"#.to_string(),
                r#""size":1000,"size":1001,"#.to_string(),
            ),
            (
                r#""unknown":{"value":1.00}"#.to_string(),
                r#""unknown":{"value":1.00},"unknown":{"value":1.00}"#.to_string(),
            ),
        ];
        for (from, duplicate) in cases {
            let body = raw.replace(&from, &duplicate);
            assert_ne!(body, raw, "{from}");
            assert!(super::classify_private_changes(body.as_bytes()).is_err());
        }
    }

    #[test]
    fn escaped_keys_and_strings_are_masked() {
        let page = r#"{"zones":[{"zoneID":{"zoneName":"PrimarySync"},"syncToken":"successor","moreComing":false,"records":[{"recordName":"master-1","recordType":"CPLMaster","recordChangeTag":"tag-1","fields":{"resOriginalRes":{"type":"ASSETID","value":{"size":1000,"download\u0055RL":"https:\/\/p01.icloud-content.com\/asset-1?x=1","fileChecksum":"checksum-A","referenceChecksum":"reference-A","wrappingKey":"wrapping-A"}}}}]}]}"#;
        let evidence = classify_ok(page.as_bytes());
        let masked = masked_text(&evidence);
        assert!(masked.contains(r#"download\u0055RL"#));
        assert!(!masked.contains("icloud-content.com"));
        assert_eq!(evidence.eligible_url_tokens, 1);
    }

    #[test]
    fn whitespace_and_numeric_lexemes_are_preserved() {
        let raw = valid_page();
        for body in [
            format!(" \n\t{raw}"),
            format!("{raw}\n\t "),
            format!("{raw}\r\n"),
        ] {
            let evidence = classify_ok(body.as_bytes());
            assert_eq!(evidence.eligible_url_tokens, 1);
            assert!(!masked_text(&evidence).contains(VALID_URL));
        }

        let spaced = raw
            .replace(r#""records":["#, r#""records" : ["#)
            .replace(r#""size":1000"#, r#""size" : 1000"#)
            .replace(r#""downloadURL":"#, r#""downloadURL" :"#);
        let evidence = classify_ok(spaced.as_bytes());
        assert_eq!(evidence.eligible_url_tokens, 1);
        let masked = masked_text(&evidence);
        assert!(masked.contains(r#""size" : 1000"#));
        assert!(!masked.contains(VALID_URL));

        let changed_lexeme = classify_ok(&with(r#""value":1.00"#, r#""value":1.0"#));
        let base = classify_ok(raw.as_bytes());
        assert_ne!(changed_lexeme.masked, base.masked);
    }

    #[test]
    fn invalid_urls_are_never_masked_or_rewritten() {
        for url in [
            "http://p01.icloud-content.com/asset-1",
            "https://evil.example.com/asset-1",
            "https://p01.icloud-content.com.evil.example/asset-1",
            "https:///asset-1",
        ] {
            assert_not_eligible(&page_with_url(url));
        }

        let invalid = "https://evil.example.com/asset-1";
        let mixed = format!(
            r#"{{"zones":[{{"zoneID":{{"zoneName":"PrimarySync"}},"syncToken":"successor","moreComing":false,"records":[{{"recordName":"a","recordType":"CPLMaster","recordChangeTag":"t","fields":{{"resOriginalRes":{{"type":"ASSETID","value":{{"size":1,"downloadURL":"{VALID_URL}","fileChecksum":"ca","referenceChecksum":"ra","wrappingKey":"wa"}}}}}}}},{{"recordName":"b","recordType":"CPLMaster","recordChangeTag":"t","fields":{{"resOriginalRes":{{"type":"ASSETID","value":{{"size":2,"downloadURL":"{invalid}","fileChecksum":"cb","referenceChecksum":"rb","wrappingKey":"wb"}}}}}}}}]}}]}}"#
        );
        let evidence = classify_ok(mixed.as_bytes());
        assert_eq!(evidence.eligible_url_tokens, 1);
        let masked = masked_text(&evidence);
        assert!(!masked.contains(VALID_URL));
        assert!(masked.contains(invalid));
    }

    #[test]
    fn malformed_oversized_and_deep_json_are_bounded() {
        for body in [
            br#"{"zones":["#.as_slice(),
            br#"{"zones":[]} trailing"#,
            br#"{"zones":[],}"#,
        ] {
            assert!(super::classify_private_changes(body).is_err());
        }

        let oversized = vec![b' '; super::super::inbox::MAX_CHANGES_PAGE_BYTES + 1];
        assert!(super::classify_private_changes(&oversized).is_err());

        let mut deep = String::new();
        for _ in 0..=super::MAX_JSON_DEPTH {
            deep.push('[');
        }
        deep.push('0');
        for _ in 0..=super::MAX_JSON_DEPTH {
            deep.push(']');
        }
        assert!(super::classify_private_changes(deep.as_bytes()).is_err());
    }

    #[test]
    fn no_eligible_descriptor_is_raw_identity() {
        for body in [
            br#"{"zones":[]}"#.as_slice(),
            br#"{"zones":[{"records":[]}]}"#,
            br#"{"zones":[{"syncToken":"successor","moreComing":false,"records":[{"recordName":"master-1","recordType":"CPLMaster","recordChangeTag":"tag-1","fields":{}}]}]}"#,
        ] {
            assert_not_eligible(body);
        }
    }

    #[test]
    fn thousand_variants_have_bounded_masked_length() {
        let mut records = Vec::new();
        let mut removed = 0usize;
        for index in 0..1000 {
            let url = format!("https://p01.icloud-content.com/asset-{index}?token={index:04}");
            removed += url.len() + 2;
            records.push(format!(
                r#"{{"recordName":"master-{index}","recordType":"CPLMaster","recordChangeTag":"tag-{index}","fields":{{"resOriginalRes":{{"type":"ASSETID","value":{{"size":{},"downloadURL":"{url}","fileChecksum":"checksum-{index}","referenceChecksum":"reference-{index}","wrappingKey":"wrapping-{index}"}}}}}}}}"#,
                index + 1
            ));
        }
        let page = format!(
            r#"{{"zones":[{{"zoneID":{{"zoneName":"PrimarySync"}},"syncToken":"successor","moreComing":false,"records":[{}]}}]}}"#,
            records.join(",")
        );
        let evidence = classify_ok(page.as_bytes());
        assert_eq!(evidence.eligible_url_tokens, 1000);
        assert_eq!(
            evidence.masked.len(),
            page.len() - removed + 1000 * super::MASKED_URL_TOKEN.len()
        );
        assert!(evidence.masked.len() < page.len());
        assert!(!masked_text(&evidence).contains("https://"));
    }

    fn root_lookalike(record_name: &str, url: &str, complete: bool) -> String {
        let tuple = if complete {
            json!({"fileChecksum":"f","referenceChecksum":"r","wrappingKey":"w","downloadURL":url})
        } else {
            json!({"fileChecksum":"f","downloadURL":url})
        };
        json!({
            "recordName": record_name, "recordType": "CPLMaster", "recordChangeTag": "t",
            "fields": {"resOriginalRes": {"type": "ASSETID",
                "value": {"size": 1, "fileChecksum": tuple["fileChecksum"],
                    "referenceChecksum": tuple.get("referenceChecksum"),
                    "wrappingKey": tuple.get("wrappingKey"), "downloadURL": url}}}
        })
        .to_string()
    }

    fn changes_with_root_lookalike(url: &str, complete: bool) -> Vec<u8> {
        valid_page()
            .replacen(
                r#"{"zones":"#,
                &format!(
                    r#"{{"records":[{}],"zones":"#,
                    root_lookalike("root-lookalike", url, complete)
                ),
                1,
            )
            .into_bytes()
    }

    fn lookup_with_nested_lookalike(url: &str, complete: bool) -> Vec<u8> {
        let mut value = json!({"size": 2048, "fileChecksum": "compute-checksum",
            "downloadURL": url});
        if complete {
            value["referenceChecksum"] = json!("compute-reference");
            value["wrappingKey"] = json!("compute-wrapping");
        }
        let child = json!({
            "recordName": "asset-1", "recordType": "CPLAsset", "recordChangeTag": "asset-tag-1",
            "fields": {"resComputeStateData": {"type": "ASSETID", "value": value}}
        })
        .to_string();
        String::from_utf8(lookup_page(VALID_URL, VALID_URL))
            .unwrap()
            .replacen(
                r#"{"records":"#,
                &format!(r#"{{"zones":[{{"records":[{child}]}}],"records":"#),
                1,
            )
            .into_bytes()
    }

    #[test]
    fn changes_envelope_never_borrows_root_lookalike_eligibility() {
        let first_url = "https://p07.icloud-content.com/root-lookalike?token=1";
        let second_url = "https://p08.icloud-content.com/root-lookalike?token=2";
        let base = classify_ok(&changes_with_root_lookalike(first_url, true));
        assert_eq!(base.eligible_url_tokens, 1);
        let masked = masked_text(&base);
        assert!(!masked.contains(VALID_URL));
        assert!(
            masked.contains(first_url),
            "unknown-envelope URL is not masked"
        );

        let rotated = classify_ok(&changes_with_root_lookalike(second_url, true));
        assert_ne!(
            rotated.masked, base.masked,
            "changing only the unknown-envelope URL must stay distinct"
        );

        for complete in [false, true] {
            let first = classify_ok(&changes_with_root_lookalike(first_url, complete));
            let second = classify_ok(&changes_with_root_lookalike(second_url, complete));
            assert_eq!(first.eligible_url_tokens, 1, "{complete}");
            assert_ne!(
                first.masked, second.masked,
                "a lookalike sibling descriptor must be validated at its own path: {complete}"
            );
            assert!(masked_text(&first).contains(first_url), "{complete}");
            assert!(masked_text(&second).contains(second_url), "{complete}");
        }
    }

    #[test]
    fn lookup_envelope_never_borrows_nested_lookalike_eligibility() {
        let first_url = "https://p07.icloud-content.com/nested-lookalike?token=1";
        let second_url = "https://p08.icloud-content.com/nested-lookalike?token=2";
        let base = super::classify_lookup_confirmation(
            &lookup_with_nested_lookalike(first_url, true),
            "asset-1",
            "master-1",
        )
        .unwrap()
        .unwrap();
        assert_eq!(base.eligible_url_tokens, 2);
        assert!(masked_text(&base).contains(first_url));

        let rotated = super::classify_lookup_confirmation(
            &lookup_with_nested_lookalike(second_url, true),
            "asset-1",
            "master-1",
        )
        .unwrap()
        .unwrap();
        assert_ne!(
            rotated.masked, base.masked,
            "changing only the unknown-envelope URL must stay distinct"
        );

        for complete in [false, true] {
            let first = super::classify_lookup_confirmation(
                &lookup_with_nested_lookalike(first_url, complete),
                "asset-1",
                "master-1",
            )
            .unwrap()
            .unwrap();
            let second = super::classify_lookup_confirmation(
                &lookup_with_nested_lookalike(second_url, complete),
                "asset-1",
                "master-1",
            )
            .unwrap()
            .unwrap();
            assert_eq!(first.eligible_url_tokens, 2, "{complete}");
            assert_ne!(
                first.masked, second.masked,
                "a nested lookalike descriptor must be validated at its own path: {complete}"
            );
            assert!(masked_text(&first).contains(first_url), "{complete}");
            assert!(masked_text(&second).contains(second_url), "{complete}");
        }
    }
}
