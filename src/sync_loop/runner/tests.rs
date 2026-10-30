//! Cycle integration proofs, grouped by the behavior they exercise.

mod checkpoints;
mod configuration;
mod enumeration;
mod metadata;
mod recovery_sequences;

mod released_upgrade;

mod mixed_provider_shapes;

#[cfg(target_os = "linux")]
mod process_death;

#[cfg(target_os = "linux")]
mod readonly_replacement;

mod primary_layout;

mod mixed_delta;
mod retained_recovery;

mod indexing;

mod trusted_legacy;

mod distinct_resource_proof;
mod shared_keys;

mod sparse_recovery;

mod unprepared_recovery;

mod v027_baseline;
