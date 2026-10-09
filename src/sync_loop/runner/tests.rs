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

mod smart_reconciliation;

mod primary_layout;
