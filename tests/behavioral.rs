//! Offline behavioral tests use the real binary, files, and seeded SQLite state.
//!
//! This remains one Cargo integration target: `cargo test --test behavioral`.
//! Test paths change from `<name>` to `<area>::<name>` for `configuration`,
//! `authentication`, `validation`, `output`, and `state`. Leaf names, assertions,
//! and platform or feature attributes are unchanged. `common::` tests keep their
//! paths. Existing leaf-name filters still work; exact filters need the area.
//!
//! Each area owns its assertions and local helpers. Areas do not import each
//! other. Only fixtures used across areas live in `support`; the facade retains
//! their private paths. The existing `common` module remains unchanged.

// Test fixtures and assertions use panics and controlled numeric conversions.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unimplemented,
    clippy::print_stderr,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss
)]

mod common;

#[path = "behavioral/authentication.rs"]
mod authentication;
#[path = "behavioral/configuration.rs"]
mod configuration;
#[path = "behavioral/output.rs"]
mod output;
#[path = "behavioral/state.rs"]
mod state;
#[path = "behavioral/support.rs"]
mod support;
#[path = "behavioral/validation.rs"]
mod validation;

use support::{
    HELPER_SCHEMA_VERSION, clean_cmd, create_state_db, insert_asset, sanitize_username,
    sync_cmd_for_config_body, sync_cmd_for_validation, write_fake_two_factor_config,
    write_sync_config,
};
