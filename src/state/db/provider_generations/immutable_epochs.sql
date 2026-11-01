-- REVIEW ONLY. Migration, authenticated insertion, charging, linked readers,
-- per-epoch proof validation, and publication-safe retirement are not wired.
CREATE TABLE provider_410_failure_epochs (
 obligation_digest TEXT NOT NULL, deadline INTEGER NOT NULL CHECK(deadline>0),
 generation TEXT NOT NULL, pass_key TEXT NOT NULL, child TEXT NOT NULL,
 version_size TEXT NOT NULL, path TEXT NOT NULL,
 attempt INTEGER NOT NULL CHECK(attempt>0), marker_hash TEXT NOT NULL,
 route_id TEXT NOT NULL, state_id TEXT NOT NULL, master TEXT NOT NULL,
 checksum TEXT NOT NULL, size_bytes INTEGER NOT NULL CHECK(size_bytes>=0),
 url TEXT NOT NULL, charged_bytes INTEGER NOT NULL CHECK(charged_bytes>0),
 PRIMARY KEY(obligation_digest,deadline,generation,pass_key,child,version_size,path,attempt,marker_hash)
) WITHOUT ROWID;
CREATE INDEX idx_provider_410_epoch_parent ON provider_410_failure_epochs(generation,pass_key,child);
CREATE TRIGGER provider_410_epoch_no_update BEFORE UPDATE ON provider_410_failure_epochs BEGIN SELECT RAISE(ABORT,'retained retry epoch'); END;
CREATE TRIGGER provider_410_epoch_no_delete BEFORE DELETE ON provider_410_failure_epochs BEGIN SELECT RAISE(ABORT,'retained retry epoch'); END;
CREATE TRIGGER provider_410_epoch_parent_retain BEFORE DELETE ON provider_active_decisions
WHEN EXISTS(SELECT 1 FROM provider_410_failure_epochs e WHERE e.generation=OLD.generation AND e.pass_key=OLD.pass_key AND e.child=OLD.child)
BEGIN SELECT RAISE(ABORT,'retained retry epoch parent'); END;
CREATE TRIGGER provider_410_epoch_root_retain BEFORE DELETE ON provider_active_generations
WHEN EXISTS(SELECT 1 FROM provider_410_failure_epochs e WHERE e.generation=OLD.id)
BEGIN SELECT RAISE(ABORT,'retained retry epoch root'); END;
CREATE TABLE provider_410_shared_waits (
 generation TEXT NOT NULL, pass_key TEXT NOT NULL, child TEXT NOT NULL,
 version_size TEXT NOT NULL, path TEXT NOT NULL, source_generation TEXT NOT NULL,
 source_attempt INTEGER NOT NULL, source_deadline INTEGER NOT NULL,
 source_marker TEXT NOT NULL, source_route TEXT NOT NULL, target_route TEXT NOT NULL,
 target_key_hash TEXT NOT NULL, target_manifest TEXT NOT NULL, target_admission TEXT NOT NULL,
 source_manifest TEXT NOT NULL, source_admission TEXT NOT NULL, policy_hash TEXT NOT NULL, policy_preimage BLOB NOT NULL, policy_snapshot BLOB NOT NULL,
 proof_hash TEXT NOT NULL, charged_bytes INTEGER NOT NULL CHECK(charged_bytes>0),
 PRIMARY KEY(generation,pass_key,child,version_size,path,source_generation,source_attempt,source_deadline,source_marker)
) WITHOUT ROWID;
CREATE TRIGGER provider_410_wait_no_update BEFORE UPDATE ON provider_410_shared_waits BEGIN SELECT RAISE(ABORT,'retained shared wait'); END;
CREATE TRIGGER provider_410_wait_no_delete BEFORE DELETE ON provider_410_shared_waits BEGIN SELECT RAISE(ABORT,'retained shared wait'); END;
CREATE TRIGGER provider_410_wait_parent_retain BEFORE DELETE ON provider_active_decisions
WHEN EXISTS(SELECT 1 FROM provider_410_shared_waits w WHERE w.generation=OLD.generation AND w.pass_key=OLD.pass_key AND w.child=OLD.child)
BEGIN SELECT RAISE(ABORT,'retained shared wait parent'); END;
CREATE INDEX idx_provider_410_wait_parent ON provider_410_shared_waits(generation,pass_key,child);
CREATE TRIGGER provider_410_wait_root_retain BEFORE DELETE ON provider_active_generations
WHEN EXISTS(SELECT 1 FROM provider_410_shared_waits w WHERE w.generation=OLD.id)
BEGIN SELECT RAISE(ABORT,'retained shared wait root'); END;
CREATE INDEX idx_provider_410_epoch_exact ON provider_410_failure_epochs(generation,pass_key,child,version_size,path,attempt,deadline);
