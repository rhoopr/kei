//! Non-authoritative, bounded discovery only. Every hint must be re-read and
//! fully authenticated with both policies in the final dispatch transaction.
use super::MAX_CHUNK_BYTES;
use crate::state::error::StateError;
use rusqlite::{Connection, params};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RefreshedHint {
    pub(crate) id: String,
    pub(crate) generation: String,
    pub(crate) pass: String,
    pub(crate) child: String,
    pub(crate) version: String,
    pub(crate) path: String,
    pub(crate) attempt: i64,
    pub(crate) deadline: i64,
}

/// The index and digest are routing hints, never proof that a candidate owns
/// the bytes. More than 64 retained candidates is ambiguous and holds this work.
pub(super) fn discover(
    conn: &Connection,
    digest: &str,
    _now: i64,
) -> Result<Vec<RefreshedHint>, StateError> {
    if conn.is_autocommit()
        || digest.len() != 64
        || !digest
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(StateError::ProviderSelectionInvalid);
    }
    // Preflight lengths without allocating untrusted persisted text or BLOBs.
    type CandidateLengths = (i64, i64, i64, i64, i64, i64, i64, i64);
    let lengths: Vec<CandidateLengths> = conn.prepare(
        "SELECT length(CAST(id AS BLOB)),length(CAST(generation AS BLOB)),length(CAST(pass_key AS BLOB)),length(CAST(child AS BLOB)),length(CAST(version_size AS BLOB)),length(CAST(path AS BLOB)),length(CAST(receipt AS BLOB)),length(CAST(proof_hash AS BLOB)) FROM provider_refreshed_transfer WHERE obligation_digest=?1 AND phase='expired' ORDER BY deadline,id LIMIT 65"
    )?.query_map(params![digest], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?,r.get(6)?,r.get(7)?)))?.collect::<Result<_,_>>()?;
    if lengths.len() > 64 {
        return Err(StateError::ProviderSelectionFull);
    }
    for (id, g, p, c, v, path, receipt, hash) in lengths {
        if id != 36
            || hash != 64
            || [g, p, c, v, path, receipt]
                .into_iter()
                .any(|n| n <= 0 || usize::try_from(n).map_or(true, |n| n > MAX_CHUNK_BYTES))
        {
            return Err(StateError::ProviderSelectionInvalid);
        }
    }
    let hints = conn.prepare(
        "SELECT id,generation,pass_key,child,version_size,path,attempt,deadline FROM provider_refreshed_transfer WHERE obligation_digest=?1 AND phase='expired' ORDER BY deadline,id LIMIT 65"
    )?.query_map(params![digest], |r| Ok(RefreshedHint {
        id:r.get(0)?, generation:r.get(1)?, pass:r.get(2)?, child:r.get(3)?,
        version:r.get(4)?, path:r.get(5)?, attempt:r.get(6)?, deadline:r.get(7)?,
    }))?.collect::<Result<Vec<_>,_>>()?;
    if hints.iter().any(|hint| {
        uuid::Uuid::parse_str(&hint.id).is_err() || hint.attempt <= 0 || hint.deadline <= 0
    }) {
        return Err(StateError::ProviderSelectionInvalid);
    }
    Ok(hints)
}
