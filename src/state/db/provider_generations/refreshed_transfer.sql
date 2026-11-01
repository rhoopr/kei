-- Source-only design prototype. Do not migrate or dispatch until the
-- authenticated Rust transaction and restart policy have been reviewed.
CREATE TABLE provider_refreshed_transfer (
  id TEXT PRIMARY KEY,
  obligation_digest TEXT NOT NULL,
  generation TEXT NOT NULL,
  pass_key TEXT NOT NULL,
  child TEXT NOT NULL,
  version_size TEXT NOT NULL,
  path TEXT NOT NULL,
  native_slot TEXT NOT NULL,
  receipt BLOB NOT NULL,
  proof_hash TEXT NOT NULL,
  prior_attempts INTEGER NOT NULL CHECK(prior_attempts BETWEEN 0 AND 32),
  policy_snapshot BLOB NOT NULL CHECK(length(policy_snapshot)>0),
  retry_delay INTEGER NOT NULL CHECK(retry_delay BETWEEN 1 AND 86400),
  phase TEXT NOT NULL CHECK(phase IN ('inflight','expired','published','failed')),
  attempt INTEGER,
  deadline INTEGER,
  outcome_proof TEXT,
  success_receipt_id TEXT,
  recovery_not_before INTEGER NOT NULL DEFAULT 0 CHECK(recovery_not_before>=0),
  recovery_batch TEXT,
  failure_status INTEGER,
  failure_error_hash TEXT,
  charged_bytes INTEGER NOT NULL CHECK(charged_bytes>0),
  CHECK((phase='inflight' AND attempt IS NULL AND deadline IS NULL AND outcome_proof IS NULL AND success_receipt_id IS NULL AND failure_status IS NULL AND failure_error_hash IS NULL)
     OR (phase='expired' AND attempt IS NOT NULL AND attempt>0 AND deadline IS NOT NULL AND deadline>0 AND outcome_proof IS NOT NULL AND length(outcome_proof)=64 AND success_receipt_id IS NULL AND failure_status IS NULL AND failure_error_hash IS NULL)
     OR (phase='published' AND attempt IS NULL AND deadline IS NULL AND outcome_proof IS NOT NULL AND length(outcome_proof)=64 AND success_receipt_id IS NOT NULL AND length(success_receipt_id)=64 AND failure_status IS NULL AND failure_error_hash IS NULL)
     OR (phase='failed' AND attempt IS NULL AND deadline IS NULL AND success_receipt_id IS NULL AND outcome_proof IS NOT NULL AND length(outcome_proof)=64 AND failure_status IS NOT NULL AND (failure_status=429 OR failure_status BETWEEN 500 AND 599) AND failure_error_hash IS NOT NULL AND length(failure_error_hash)=64))
) WITHOUT ROWID;
-- The caller must derive native_slot with the current normalized publication
-- owner and recheck alias/owner guards; this index alone proves no filesystem
-- exclusivity. The recorded path remains untouched. No timer steals a row:
-- after crash it remains ambiguous until an independently authenticated repair.
CREATE UNIQUE INDEX provider_refreshed_one_inflight_path
  ON provider_refreshed_transfer(native_slot) WHERE phase='inflight';
CREATE INDEX provider_refreshed_future_candidates
  ON provider_refreshed_transfer(obligation_digest,phase,deadline,id);
CREATE INDEX provider_refreshed_parent
  ON provider_refreshed_transfer(generation,pass_key,child);
CREATE TRIGGER provider_refreshed_immutable_source
BEFORE UPDATE OF id,obligation_digest,generation,pass_key,child,version_size,path,native_slot,receipt,proof_hash,prior_attempts,policy_snapshot,retry_delay,charged_bytes
ON provider_refreshed_transfer BEGIN SELECT RAISE(ABORT,'immutable refreshed source'); END;
CREATE TRIGGER provider_refreshed_outcome_once
BEFORE UPDATE OF phase,attempt,deadline,outcome_proof,success_receipt_id,failure_status,failure_error_hash ON provider_refreshed_transfer
WHEN OLD.phase!='inflight' OR NEW.phase NOT IN ('expired','published','failed')
BEGIN SELECT RAISE(ABORT,'immutable refreshed outcome'); END;
CREATE TRIGGER provider_refreshed_no_delete BEFORE DELETE ON provider_refreshed_transfer
BEGIN SELECT RAISE(ABORT,'retained refreshed evidence'); END;
CREATE TRIGGER provider_refreshed_parent_retain BEFORE DELETE ON provider_active_decisions
WHEN EXISTS(SELECT 1 FROM provider_refreshed_transfer e
 WHERE e.generation=OLD.generation AND e.pass_key=OLD.pass_key AND e.child=OLD.child)
BEGIN SELECT RAISE(ABORT,'retained refreshed parent'); END;
CREATE TRIGGER provider_refreshed_root_retain BEFORE DELETE ON provider_active_generations
WHEN EXISTS(SELECT 1 FROM provider_refreshed_transfer e WHERE e.generation=OLD.id)
BEGIN SELECT RAISE(ABORT,'retained refreshed root'); END;
CREATE TRIGGER provider_refreshed_destination_retain BEFORE DELETE ON provider_active_destinations
WHEN EXISTS(SELECT 1 FROM provider_refreshed_transfer e
 WHERE e.generation=OLD.generation AND e.pass_key=OLD.pass_key AND e.child=OLD.child
 AND e.version_size=OLD.version_size AND e.path=OLD.path)
BEGIN SELECT RAISE(ABORT,'retained refreshed destination'); END;
-- A shared wait records the target's authenticated lookup and the particular
-- predecessor epoch, without inventing an attempt for the target generation.
CREATE TABLE provider_refreshed_waits (
 generation TEXT NOT NULL, pass_key TEXT NOT NULL, child TEXT NOT NULL,
 version_size TEXT NOT NULL, path TEXT NOT NULL, source_kind TEXT NOT NULL
 CHECK(source_kind IN ('original','refreshed')), source_id TEXT NOT NULL,
 deadline INTEGER NOT NULL CHECK(deadline>0), receipt BLOB NOT NULL,
 policy_snapshot BLOB NOT NULL, proof_hash TEXT NOT NULL,
 charged_bytes INTEGER NOT NULL CHECK(charged_bytes>0),
 PRIMARY KEY(generation,pass_key,child,version_size,path,source_kind,source_id)
) WITHOUT ROWID;
CREATE TRIGGER provider_refreshed_wait_immutable BEFORE UPDATE ON provider_refreshed_waits
BEGIN SELECT RAISE(ABORT,'immutable refreshed wait'); END;
CREATE TRIGGER provider_refreshed_wait_retain BEFORE DELETE ON provider_refreshed_waits
BEGIN SELECT RAISE(ABORT,'retained refreshed wait'); END;
CREATE TRIGGER provider_refreshed_wait_parent_retain BEFORE DELETE ON provider_active_decisions
WHEN EXISTS(SELECT 1 FROM provider_refreshed_waits w WHERE w.generation=OLD.generation AND w.pass_key=OLD.pass_key AND w.child=OLD.child)
BEGIN SELECT RAISE(ABORT,'retained refreshed wait parent'); END;
CREATE TRIGGER provider_refreshed_wait_root_retain BEFORE DELETE ON provider_active_generations
WHEN EXISTS(SELECT 1 FROM provider_refreshed_waits w WHERE w.generation=OLD.id)
BEGIN SELECT RAISE(ABORT,'retained refreshed wait root'); END;
-- Preserve the exact authenticated source receipt observed at completion.
-- Later ordinary metadata/checksum updates may change download_receipts, so
-- the terminal proof never depends on that mutable row remaining unchanged.
CREATE TABLE provider_refreshed_success_receipts (
 dispatch_id TEXT PRIMARY KEY,
 receipt_id TEXT NOT NULL CHECK(length(receipt_id)=64),
 receipt BLOB NOT NULL CHECK(length(receipt)>0),
 proof_hash TEXT NOT NULL CHECK(length(proof_hash)=64),
 charged_bytes INTEGER NOT NULL CHECK(charged_bytes>0)
) WITHOUT ROWID;
CREATE TRIGGER provider_refreshed_success_immutable BEFORE UPDATE ON provider_refreshed_success_receipts
BEGIN SELECT RAISE(ABORT,'immutable refreshed success receipt'); END;
CREATE TRIGGER provider_refreshed_success_retain BEFORE DELETE ON provider_refreshed_success_receipts
BEGIN SELECT RAISE(ABORT,'retained refreshed success receipt'); END;
CREATE TRIGGER provider_refreshed_success_requires_snapshot
BEFORE UPDATE OF phase ON provider_refreshed_transfer
WHEN NEW.phase='published' AND NOT EXISTS(
 SELECT 1 FROM provider_refreshed_success_receipts s
 WHERE s.dispatch_id=OLD.id AND s.receipt_id=NEW.success_receipt_id
)
BEGIN SELECT RAISE(ABORT,'missing immutable success receipt'); END;

-- Due order is a scheduling hint only; no outcome or ownership is inferred.
CREATE INDEX provider_refreshed_recovery_due
 ON provider_refreshed_transfer(generation,recovery_not_before,id) WHERE phase='inflight';
CREATE INDEX provider_refreshed_recovery_batch
 ON provider_refreshed_transfer(generation,recovery_batch,id) WHERE phase='inflight';
