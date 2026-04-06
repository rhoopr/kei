# PR Review: `fix/robustness`

**Date:** 2026-04-06
**Branch:** `fix/robustness` → `main`
**Commits:** 80
**Changed files:** 60 (51 `.rs` files)
**Lines:** +9,964 / −2,704

> **Post-review update:** CF-1, CF-2, NB-1, NB-2, NB-12, and NB-18 have been fixed
> in commit `5fd50ac`. Merge verdict updated to **APPROVE WITH COMMENTS**.

---

## PR Summary

This branch is a comprehensive robustness overhaul of the kei photo sync engine, bumping to v0.6.0. It introduces credential hardening (SecretString, keyring integration, AES-GCM encrypted file storage), download pipeline improvements (content validation, atomic writes, EXIF on .part files, DirCache, retry state writes), iCloud API resilience (per-record error filtering, cross-page master buffering, sync token preservation), database migration safety (SAVEPOINTs, crash recovery), process hardening (core dump suppression, disk space checks, health status files), and extensive new test infrastructure (wiremock HTTP tests, MockPhotosSession, TestAssetRecord builders, tracing-test). The branch also resolves 188 clippy pedantic warnings and migrates shared immutable data to `Arc<str>`/`Arc<Value>`. Overall risk profile: **merge with fixes** — two High-severity issues must be addressed, but the branch is structurally sound.

---

## Merge Verdict

**APPROVE WITH COMMENTS** — The two High-severity findings (CF-1, CF-2) and four prioritized non-blocking findings (NB-1, NB-2, NB-12, NB-18) have been resolved. Remaining non-blocking items are low-priority cleanup.

---

## Critical Findings

**[CF-1] `sanitize_username` panics on multi-byte Unicode when truncating** *(Introduced)*
- **Location:** `src/auth/session.rs:55`
- **Severity:** High
- **Effort:** Trivial
- **Description:** `sanitized` retains Unicode alphanumeric characters (CJK, Cyrillic, etc.) via `c.is_alphanumeric()`. When `sanitized.len() > 64`, `prefix_len` is computed as 47 (byte offset). `&sanitized[..47]` slices at byte index 47, which can land mid-character for multi-byte UTF-8 sequences, causing a panic. Example: 22 CJK characters = 66 bytes; byte 47 splits the 16th character (3-byte chars at offsets 45..48).
- **Recommendation:**
  ```rust
  // Before:
  format!("{}_{:016x}", &sanitized[..prefix_len], hash)

  // After — find the nearest char boundary at or before prefix_len:
  let prefix_end = sanitized[..prefix_len]
      .char_indices()
      .last()
      .map(|(i, c)| i + c.len_utf8())
      .unwrap_or(prefix_len);
  format!("{}_{:016x}", &sanitized[..prefix_end], hash)
  ```

---

**[CF-2] `clear_password_env()` calls `unsafe { remove_var }` after tokio runtime is active** *(Introduced)*
- **Location:** `src/config.rs:307`, called from `src/main.rs:990`
- **Severity:** High
- **Effort:** Moderate
- **Description:** The SAFETY comment states "called during single-threaded init before tokio spawns workers." This is false — `clear_password_env()` is called inside `async fn run()` which executes within `#[tokio::main]`, after signal handlers are spawned. `std::env::remove_var` is `unsafe` since Rust 1.66 because concurrent access is undefined behavior per POSIX. While glibc/macOS implementations use internal mutexes, this is technically UB and the safety invariant is incorrect.
- **Recommendation:** Move the call before the tokio runtime starts by building the runtime manually:
  ```rust
  fn main() -> ExitCode {
      // Safe: truly single-threaded here, before runtime creation
      unsafe { std::env::remove_var("ICLOUD_PASSWORD") };

      tokio::runtime::Builder::new_multi_thread()
          .enable_all()
          .build()
          .expect("Failed to build tokio runtime")
          .block_on(run())
  }
  ```
  Or read the env var into `Config` and drop the env reference during `Config::build()`, before the unsafe call point, and move the call to before `tokio::main`.

---

## Non-Blocking Findings

### Area 1: Correctness & Panics

**[NB-1] `credential.rs` `file_delete()` logic: returns Ok even when nothing deleted** *(Introduced)*
- **Location:** `src/credential.rs:243-249`
- **Severity:** Low
- **Effort:** Trivial
- **Description:** `file_delete()` returns `Ok(())` when the credential file doesn't exist. This causes `delete()` to set `deleted = true` even when no file was removed. If neither keyring nor file backend has a credential, the method returns `Ok(())` instead of the "No stored credential found" error.
- **Recommendation:** Have `file_delete()` return `Ok(bool)` indicating whether a file was actually removed, and only set `deleted = true` when the return is `Ok(true)`.

**[NB-2] Operator precedence ambiguity in HTML detection** *(Introduced)*
- **Location:** `src/download/file.rs:381`
- **Severity:** Low
- **Effort:** Trivial
- **Description:** `starts_with(b"<!") || trimmed.len() >= 5 && ...` relies on `&&` binding tighter than `||`. The result is correct but fragile — add explicit parentheses.
- **Recommendation:**
  ```rust
  if trimmed.starts_with(b"<!")
      || (trimmed.len() >= 5 && trimmed[..5].eq_ignore_ascii_case(b"<html"))
  ```

**[NB-3] `HealthStatus::consecutive_failures` can overflow on `+= 1`** *(Introduced)*
- **Location:** `src/health.rs:34`
- **Severity:** Low
- **Effort:** Trivial
- **Description:** Debug mode panics on u32 overflow; release mode wraps to 0 (falsely indicating zero failures). Use `saturating_add(1)`.

**[NB-4] `HealthStatus::write` uses synchronous `std::fs::write` on async runtime** *(Introduced)*
- **Location:** `src/health.rs:45`
- **Severity:** Low
- **Effort:** Trivial
- **Description:** Called from the main sync loop on a tokio worker. For a tiny JSON file written infrequently this is negligible. Accept as-is or use `tokio::fs::write`.

**[NB-5] `shutdown.rs` — `std::process::exit(130)` bypasses Drop destructors** *(Exposed)*
- **Location:** `src/shutdown.rs:72-77`
- **Severity:** Low
- **Effort:** Moderate
- **Description:** `process::exit` skips `PidFileGuard` drop (introduced in this branch), leaving stale PID files. Intentional for force-exit but worth documenting.

**[NB-6] SQL LIKE metacharacter injection in `delete_metadata_by_prefix`** *(Introduced)*
- **Location:** `src/state/db.rs:648`
- **Severity:** Low
- **Effort:** Trivial
- **Description:** If `prefix` contains `%` or `_`, LIKE matches unintended keys. All current callers pass hardcoded strings, but the method is `pub`. Use `GLOB ?1` with `format!("{prefix}*")` instead.

**[NB-7] `column_exists` uses string interpolation for table name in SQL** *(Introduced)*
- **Location:** `src/state/schema.rs:103`
- **Severity:** Low
- **Effort:** Trivial
- **Description:** `format!("PRAGMA table_info({table})")` — all callers pass hardcoded `"assets"`. Add a `debug_assert!` validating the table name is a simple identifier.

### Area 2: Algorithmic & Allocation

**[NB-8] Unbounded `pending_masters` growth in `spawn_fetcher`** *(Introduced)*
- **Location:** `src/icloud/photos/album.rs:479`
- **Severity:** Medium
- **Effort:** Moderate
- **Description:** Cross-page master buffering has no cap. A pathological API response pattern (many masters without matching assets) could grow the HashMap unbounded. Add a capacity guard (e.g., `page_size * 10`) with a warning log and drain.

**[NB-9] Missing `pending_assets` buffer in `spawn_fetcher`** *(Introduced)*
- **Location:** `src/icloud/photos/album.rs:590-604`
- **Severity:** Medium
- **Effort:** Moderate
- **Description:** When a page has CPLAsset records but no matching CPLMaster, the assets are discarded. If the master arrives on a later page, it won't find its asset. The `DeltaRecordBuffer` already handles this bidirectionally — `spawn_fetcher` should too.

**[NB-10] Missing `with_capacity` on per-page allocations** *(Introduced)*
- **Location:** `src/icloud/photos/album.rs:564-565`
- **Severity:** Low
- **Effort:** Trivial
- **Description:** `page_assets` and `page_masters` are allocated fresh per page without capacity hints despite `record_count` being known. Use `HashMap::with_capacity(record_count / 2)`.

### Area 3: Async & Concurrency

**[NB-11] Blocking sync I/O (`validate_downloaded_content`) on async task** *(Introduced)*
- **Location:** `src/download/file.rs:289`
- **Severity:** Medium
- **Effort:** Moderate
- **Description:** `validate_downloaded_content` calls `std::fs::File::open()` and `std::io::Read::read()` on a tokio worker thread. For a 16-byte read on local disk this is fast, but on NAS or under I/O contention with concurrent downloads, it can stall the runtime. Wrap in `spawn_blocking` for consistency with the rest of the pipeline.

**[NB-12] `DirCache::ensure_dir_async` silently swallows `JoinError`** *(Introduced)*
- **Location:** `src/download/paths.rs:469-475`
- **Severity:** Medium
- **Effort:** Trivial
- **Description:** `spawn_blocking(...).await.unwrap_or_default()` swallows panics/cancellations. An empty DirCache entry causes re-downloads of every file in that directory. Use `unwrap_or_else` with a `tracing::warn`.

**[NB-13] `Notifier::notify()` spawns fire-and-forget tasks — panics silently lost** *(Introduced)*
- **Location:** `src/notifications.rs:69`
- **Severity:** Low
- **Effort:** Trivial
- **Description:** The `JoinHandle` is dropped. Intentional fire-and-forget design, but consider logging panics via a wrapper task.

### Area 4: Public API

**[NB-14] `pub mod retry` should be `pub(crate)`** *(Introduced)*
- **Location:** `src/main.rs:20`
- **Severity:** Medium
- **Effort:** Trivial
- **Description:** Exposes `RetryConfig`, `RetryAction`, and `retry_with_backoff` as public API in a binary crate. Change to `pub(crate) mod retry;`.

**[NB-15] Multiple `pub` items that should be `pub(crate)`** *(Introduced/Exposed)*
- **Locations:**
  - `src/health.rs` — `HealthStatus` methods/fields
  - `src/notifications.rs` — `Event`, `Notifier`
  - `src/migration.rs` — `MigrationReport`, `migrate_legacy_paths`
  - `src/download/mod.rs:43,126,136,151` — `determine_media_type`, `DownloadOutcome`, `SyncMode`, `SyncResult`
  - `src/icloud/photos/album.rs:50` — `PhotoAlbumConfig` and fields
  - `src/icloud/photos/mod.rs:6-12` — `cloudkit`, `queries`, `session`, `types` submodules
- **Severity:** Low
- **Effort:** Trivial
- **Description:** Per project code style ("pub(crate) internal, pub for module API only"), these should be narrowed.

**[NB-21] `CredentialStore` and `PasswordSource` missing `Debug` derive** *(Introduced)*
- **Locations:** `src/credential.rs:29`, `src/password.rs:23`
- **Severity:** Low
- **Effort:** Trivial
- **Description:** Project code style says "Derive `Debug` always." Both new types lack it. `PasswordSource` contains a `SecretString` variant, so a manual `Debug` impl that redacts it is appropriate.

**[NB-22] `classify_auth_http_error` and `reject_on_rscd` untested** *(Introduced)*
- **Locations:** `src/auth/twofa.rs:34-44`, `src/auth/twofa.rs:94-111`
- **Severity:** Medium
- **Effort:** Moderate
- **Description:** Both are new functions with branching logic. `classify_auth_http_error` has three arms (421/450, 5xx, fallback) that are entirely untested. Add unit tests for each arm.

**[NB-23] `prompt_password` calls `block_in_place` inside a sync function** *(Introduced)*
- **Location:** `src/password.rs:124-130`
- **Severity:** Low
- **Effort:** Trivial
- **Description:** `prompt_password` is `pub fn` (sync) but internally requires a tokio multi-threaded runtime via `block_in_place`. Calling from a single-threaded runtime or `spawn_blocking` will panic. Document the runtime requirement or remove `block_in_place` (the caller context already handles blocking).

### Area 5: Style / `#[allow]` Annotations

**[NB-16] ~20+ `#[allow(clippy::...)]` annotations violate project rules** *(Introduced)*
- **Locations:** Across `src/auth/session.rs`, `src/auth/srp.rs`, `src/icloud/photos/album.rs`, `src/icloud/photos/asset.rs`, `src/icloud/photos/cloudkit.rs`, `src/download/mod.rs`, `src/download/file.rs`, `src/config.rs`, `src/retry.rs`, `src/systemd.rs`, `tests/common/mod.rs`
- **Severity:** Medium (aggregate — individually Low)
- **Effort:** Moderate (some require extraction/refactoring; some are pragmatic exceptions)
- **Description:** CLAUDE.md states "No `#[allow(...)]` — fix warnings." The most common suppressions are `too_many_lines` (refactor needed), `cast_possible_truncation` (use `try_from`), `struct_field_names` (serde constraint), `needless_pass_by_value` (change to `&T`), and `dead_code` in test helpers (module-level allow is acceptable).
- **Recommendation:** Address incrementally:
  - **Remove by fixing:** `needless_pass_by_value`, `cast_possible_truncation`, `cast_sign_loss`, `unnecessary_semicolon`, `missing_errors_doc` (fix visibility first)
  - **Accept with comment:** `struct_field_names` (serde renames), `dead_code` (test modules), `struct_excessive_bools` (config struct)
  - **Refactor later:** `too_many_lines` (extract helpers)

### Area 6: Unsafe Code

**[NB-17] Missing `// SAFETY:` comments on 3 unsafe blocks** *(Introduced)*
- **Locations:** `src/main.rs:108`, `src/main.rs:114`, `src/main.rs:157`
- **Severity:** Low
- **Effort:** Trivial
- **Description:** Three `unsafe` blocks for `prctl`, `setrlimit`, and `statvfs` lack SAFETY comments. All calls are sound (correct types, stack-local lifetimes, no aliasing), but Rust convention requires documenting invariants.

**[NB-18] `make_password_provider` silently swallows errors** *(Introduced)*
- **Location:** `src/main.rs:176-179`
- **Severity:** Medium
- **Effort:** Trivial
- **Description:** `.resolve().ok().flatten()` converts file-not-found, permission errors, and command failures all to `None` with no user feedback. Add `tracing::debug!` on `Err` to make failures discoverable.

### Area 7: Dependency Changes

**[NB-19] `paste` crate unmaintained (RUSTSEC-2024-0436)** *(Pre-existing, via `little_exif`)*
- **Severity:** Low
- **Effort:** Trivial (accept as advisory)
- **Description:** Transitive dependency `paste 1.0.15` via `little_exif` is flagged as unmaintained. No security vulnerability — just a maintenance risk. Monitor for `little_exif` updates.

**New dependencies added (all justified):**
| Dependency | Purpose | Notes |
|-----------|---------|-------|
| `secrecy 0.10` | `SecretString` for password handling | Core to credential hardening |
| `aes-gcm 0.10` | Encrypted credential file storage | Appropriate for at-rest encryption |
| `keyring 3` | OS keychain integration | Platform-specific features correctly configured |
| `libc 0.2` | Process hardening syscalls | Necessary for `prctl`/`setrlimit`/`statvfs` |
| `bytes 1` | Byte buffer handling | Already a transitive dep via tokio/hyper |
| `wiremock 0.6` | HTTP mock testing (dev-dep) | Appropriate for integration tests |
| `tracing-test 0.2` | Log assertion in tests (dev-dep) | Lightweight test utility |
| `http 1` | HTTP types in tests (dev-dep) | Already a transitive dep |

**Removed:** `uuid` v1 feature (unused), `atomic`/`bytemuck` (replaced).

No duplicate transitive dependencies introduced. No semver-incompatible bumps on existing deps.

### Area 8: Migration Safety

**[NB-20] SAVEPOINT does not cover `PRAGMA user_version`** *(Introduced)*
- **Location:** `src/state/schema.rs:73-84`
- **Severity:** Low
- **Effort:** Trivial (documentation only)
- **Description:** `PRAGMA user_version` writes directly to the database header outside any transaction in WAL mode. A crash between DDL and PRAGMA could leave the version un-bumped. However, the idempotent `column_exists` checks in v3/v4 migrations handle this, and tests verify crash recovery. Update the doc comment to note this.

---

## Test Gaps

| Function / Path | Missing Scenario | Severity |
|----------------|-----------------|----------|
| `make_password_provider` / `make_provider_from_auth` (`main.rs:176-196`) | No tests at all — error path swallows silently | Advisory |
| `validate_downloaded_content` with HTML matching `expected_size` (`download/file.rs`) | HTML error page where byte count matches expected size | Advisory |
| `row_to_asset_record` unknown `version_size` / `media_type` fallbacks (`state/db.rs`) | Only `status` fallback is tested; analogous tests for the other two fields are missing | Advisory |
| `sanitize_username` with multi-byte Unicode (`auth/session.rs`) | CJK/emoji/Cyrillic inputs that trigger truncation — would catch CF-1 | Critical (blocks merge) |
| `CredentialStore::delete` when neither backend has credential (`credential.rs`) | Would expose NB-1 logic bug | Advisory |
| `pending_masters` exhaustion in `spawn_fetcher` (`icloud/photos/album.rs`) | Pathological all-masters-no-assets pages | Advisory |
| `classify_auth_http_error` (`auth/twofa.rs:34-44`) | No tests for 421/450, 5xx, or fallback arms | Advisory |
| `reject_on_rscd` (`auth/twofa.rs:94-111`) | Only tested indirectly via `check_apple_rscd` | Advisory |
| `CredentialStore::store` / `retrieve` / `backend_name` (`credential.rs`) | Public API round-trip not tested; only file-backend internals tested | Advisory |

---

## Suggested Benchmarks

| Finding | Target | Tool | Input | Metric |
|---------|--------|------|-------|--------|
| NB-8 (unbounded `pending_masters`) | `spawn_fetcher` with large libraries | `criterion` | 1M photo library simulation with 10% orphaned masters | Peak RSS via `/proc/self/status` |
| NB-11 (blocking `validate_downloaded_content`) | `attempt_download` under concurrent load | `criterion` | 16 concurrent downloads on NAS-like latency (inject 50ms per syscall) | p99 latency for download completion |
| NB-10 (missing `with_capacity`) | `spawn_fetcher` page processing | `criterion` | 10K records/page, 100 pages | allocator count via `dhat` |

---

## Clippy Status

20 pedantic-level warnings (non-blocking):
- 5× `doc_markdown` (backtick suggestions)
- 4× `default_trait_access`
- 2× `too_many_lines`
- 2× `borrow_as_ptr` (raw pointer style suggestion)
- 2× `cast_possible_wrap` / `cast_lossless`
- 2× `items_after_statements`
- 1× `redundant_closure_for_method_calls`
- 1× `unnecessary_semicolon`
- 1× `unused_async`

No errors. No `clippy::all` warnings.

## `cargo audit` Status

1 allowed advisory: RUSTSEC-2024-0436 (`paste` unmaintained, transitive via `little_exif`). No vulnerabilities.
