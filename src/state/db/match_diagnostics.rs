//! Bounded explanations of matching attempts. Never identity or retry authority.
use serde::{Deserialize, Serialize};

use super::provider_selection::SelectionPath;

const PREFIX: &str = "existing_match_v1:";
const LIMIT: usize = 16;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct MatchRefusal {
    pub(crate) path: Option<SelectionPath>,
    pub(crate) reason: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub(crate) struct MatchExplanation {
    pub(crate) candidates: Vec<MatchRefusal>,
    pub(crate) omitted: usize,
}

impl MatchExplanation {
    pub(crate) fn add(&mut self, path: Option<&std::path::Path>, reason: &str) {
        let path = path.map(SelectionPath::from_path);
        if self
            .candidates
            .iter()
            .any(|entry| entry.path == path && entry.reason == reason)
        {
            return;
        }
        if self.candidates.len() < LIMIT {
            self.candidates.push(MatchRefusal {
                path,
                reason: reason.into(),
            });
        } else {
            self.omitted = self.omitted.saturating_add(1);
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct MatchAttempt {
    pub(crate) generation: Option<String>,
    pub(crate) pass: Option<String>,
    pub(crate) child: String,
    pub(crate) destination: SelectionPath,
    pub(crate) checksum: String,
    pub(crate) observed_at: i64,
    pub(crate) explanation: MatchExplanation,
}

#[derive(Clone, Default, Serialize, Deserialize)]
pub(crate) struct MatchDiagnostics {
    #[serde(default)]
    pub(crate) last_transfer_error: Option<String>,
    pub(crate) attempts: Vec<MatchAttempt>,
    pub(crate) omitted: usize,
}

impl MatchDiagnostics {
    pub(crate) fn decode(value: &str) -> Option<Self> {
        // Diagnostic corruption must not become authority or unbounded allocation.
        let payload = value.strip_prefix(PREFIX)?;
        if payload.len() > 131_072 {
            return None;
        }
        let result: Self = serde_json::from_str(payload).ok()?;
        (result.attempts.len() <= LIMIT
            && result
                .attempts
                .iter()
                .all(|a| a.explanation.candidates.len() <= LIMIT))
        .then_some(result)
    }

    pub(crate) fn encode(&self) -> Option<String> {
        let mut bounded = self.clone();
        loop {
            let payload = serde_json::to_string(&bounded).ok()?;
            if payload.len() <= 131_072 {
                return Some(format!("{PREFIX}{payload}"));
            }
            if bounded.attempts.len() > 1 {
                bounded.attempts.remove(0);
                bounded.omitted = bounded.omitted.saturating_add(1);
            } else if let Some(attempt) = bounded.attempts.first_mut()
                && attempt.explanation.candidates.pop().is_some()
            {
                attempt.explanation.omitted = attempt.explanation.omitted.saturating_add(1);
            } else {
                return None;
            }
        }
    }

    pub(crate) fn merge(old: Option<&str>, new: &str) -> Option<String> {
        let mut prior = old.and_then(Self::decode).unwrap_or_else(|| Self {
            last_transfer_error: old
                .filter(|error| !error.starts_with(PREFIX))
                .map(|error| error.chars().take(4096).collect()),
            ..Self::default()
        });
        let incoming = Self::decode(new)?;
        for attempt in incoming.attempts {
            prior.attempts.retain(|old| {
                !(old.generation == attempt.generation
                    && old.pass == attempt.pass
                    && old.child == attempt.child
                    && old.destination == attempt.destination)
            });
            if prior.attempts.len() == LIMIT {
                prior.attempts.remove(0);
                prior.omitted = prior.omitted.saturating_add(1);
            }
            prior.attempts.push(attempt);
        }
        prior.encode()
    }

    pub(crate) fn with_transfer_error(old: Option<&str>, error: &str) -> String {
        // Explicit reconciliation sentinels keep their existing exact contract.
        // They replace an asset's prior diagnosis and are never read from an envelope.
        if matches!(
            error,
            crate::commands::reconcile::FILE_MISSING_REASON
                | crate::commands::reconcile::FILE_TRUNCATED_REASON
        ) {
            return error.into();
        }
        let Some(mut diagnostic) = old.and_then(Self::decode) else {
            return error.into();
        };
        diagnostic.last_transfer_error = Some(error.chars().take(4096).collect());
        diagnostic
            .encode()
            .unwrap_or_else(|| old.unwrap_or(error).into())
    }

    pub(crate) fn after_publication(
        old: Option<&str>,
        path: &std::path::Path,
        checksum: Option<&str>,
    ) -> Option<String> {
        let mut diagnostic = Self::decode(old?)?;
        let native = SelectionPath::from_path(&std::path::absolute(path).ok()?);
        diagnostic
            .attempts
            .retain(|a| a.destination != native || Some(a.checksum.as_str()) != checksum);
        (!diagnostic.attempts.is_empty())
            .then(|| diagnostic.encode())
            .flatten()
    }
}

pub(crate) struct PendingMatchStatus {
    pub(crate) policy_excluded: bool,
    pub(crate) attempt: MatchAttempt,
    /// Recorded schedule, not confirmation of current configuration eligibility.
    pub(crate) schedule: Option<(i64, i64)>,
    pub(crate) omitted: usize,
}

impl super::SqliteStateDb {
    pub(crate) async fn pending_match_status(
        &self,
        limit: usize,
    ) -> Result<Vec<PendingMatchStatus>, crate::state::error::StateError> {
        self.with_conn("pending_match_status", move |conn| {
            use rusqlite::{OptionalExtension, params};
            let rows: Vec<(String,String,String,String,String)> = conn.prepare(
                "SELECT library,id,version_size,last_error,status FROM assets WHERE last_error LIKE 'existing_match_v1:%' ORDER BY library,id,version_size LIMIT ?1")?
                .query_map([limit.min(1000) as i64], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?,row.get(4)?)))?
                .collect::<Result<_,_>>()?;
            let mut result = Vec::new();
            for (library,id,version,error,status) in rows {
                let Some(report) = MatchDiagnostics::decode(&error) else { continue; };
                for attempt in report.attempts {
                    let path = attempt.destination.key()?;
                    let schedule = conn.query_row(
                        "SELECT d.attempts,d.next_retry_at FROM provider_active_destinations w JOIN provider_active_decisions d USING(generation,pass_key,child) WHERE w.generation=?1 AND w.pass_key=?2 AND w.child=?3 AND w.library=?4 AND w.asset_id=?5 AND w.version_size=?6 AND w.path=?7 AND w.checksum=?8 AND w.admitted=1 AND w.verified_media=0 AND d.admission='admitted'",
                        params![attempt.generation,attempt.pass,attempt.child,library,id,version,path,attempt.checksum],
                        |row| Ok((row.get::<_,i64>(0)?,row.get::<_,i64>(1)?))).optional()?;
                    result.push(PendingMatchStatus { attempt, schedule, policy_excluded:status == "policy_excluded", omitted: report.omitted });
                }
            }
            Ok(result)
        }).await
    }
}

#[cfg(test)]
mod tests {
    use super::{MatchAttempt, MatchDiagnostics, MatchExplanation};
    use crate::state::db::provider_selection::SelectionPath;

    #[test]
    fn bounded_match_reports_preserve_native_paths_and_deduplicate_only_equal_reasons() {
        let mut report = MatchDiagnostics::default();
        for n in 0..20 {
            let path = std::path::PathBuf::from(format!("/archive/{n}.JPG"));
            let mut explanation = MatchExplanation::default();
            explanation.add(Some(&path), "proof_missing");
            explanation.add(Some(&path), "proof_missing");
            explanation.add(Some(&path), "different_refusal");
            assert_eq!(explanation.candidates.len(), 2);
            let incoming = MatchDiagnostics {
                last_transfer_error: None,
                attempts: vec![MatchAttempt {
                    generation: Some("generation".into()),
                    pass: Some("pass".into()),
                    child: "child".into(),
                    destination: SelectionPath::from_path(&path),
                    checksum: "checksum".into(),
                    observed_at: n,
                    explanation,
                }],
                omitted: 0,
            };
            report = MatchDiagnostics::decode(
                &MatchDiagnostics::merge(report.encode().as_deref(), &incoming.encode().unwrap())
                    .unwrap(),
            )
            .unwrap();
        }
        assert_eq!(report.attempts.len(), 16);
        assert_eq!(report.omitted, 4);
        let envelope = report.encode().unwrap();
        for sentinel in [
            crate::commands::reconcile::FILE_MISSING_REASON,
            crate::commands::reconcile::FILE_TRUNCATED_REASON,
        ] {
            assert_eq!(
                MatchDiagnostics::with_transfer_error(Some(&envelope), sentinel),
                sentinel
            );
        }
        report.attempts[0].explanation.candidates[0].reason = "x".repeat(131_073);
        let bounded = MatchDiagnostics::decode(&report.encode().unwrap()).unwrap();
        assert!(bounded.omitted > report.omitted);
        assert!(MatchDiagnostics::decode("existing_match_v1:{broken").is_none());
        #[cfg(unix)]
        {
            let mut report = bounded;
            use std::os::unix::ffi::OsStringExt;
            let path = std::path::PathBuf::from(std::ffi::OsString::from_vec(
                b"/archive/nonutf8-\xff.JPG".to_vec(),
            ));
            report.attempts[0].destination = SelectionPath::from_path(&path);
            assert_eq!(
                MatchDiagnostics::decode(&report.encode().unwrap())
                    .unwrap()
                    .attempts[0]
                    .destination
                    .to_path(),
                path
            );
        }
    }
}
