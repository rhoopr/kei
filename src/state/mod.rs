//! State tracking module for persistent sync state.
//!
//! This module provides SQLite-based state tracking for iCloud photo downloads.
//! It tracks which assets have been seen, downloaded, or failed, enabling:
//! - Skip-by-DB downloads (faster than filesystem checks)
//! - Failure tracking and retry
//! - Status reporting
//! - Verification of downloaded files

pub mod db;
pub mod error;
pub mod schema;
pub mod types;

pub use db::{SqliteStateDb, StateDb};
pub use types::{AssetRecord, AssetStatus, MediaType, SyncRunStats, VersionSizeKey};
