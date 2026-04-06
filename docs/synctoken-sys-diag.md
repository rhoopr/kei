# kei System Diagram

Full lifecycle of authentication, photo enumeration, incremental sync, file downloads, and SQLite state management.

---

## High-Level Architecture

```
┌──────────────────────────────────────────────────────────────────────┐
│                           CLI / Config                               │
│  src/cli.rs → src/config.rs → TOML merge → DownloadConfig            │
└──────────────────────┬───────────────────────────────────────────────┘
                       │
                       ▼
┌──────────────────────────────────────────────────────────────────────┐
│                        Main Sync Loop                                │
│                        src/main.rs                                   │
│                                                                      │
│  ┌─────────┐   ┌──────────────┐   ┌──────────────┐   ┌───────────┐   │
│  │  Auth   │──▶│ PhotosService│──▶│  Download    │──▶│  State DB │   │
│  │  Layer  │   │  (API)       │   │  Pipeline    │   │  (SQLite) │   │
│  └─────────┘   └──────────────┘   └──────────────┘   └───────────┘   │
│       │              │                    │                  │       │
│       ▼              ▼                    ▼                  ▼       │
│  Cookie jar    iCloud CloudKit      Local files       assets table   │
│  Lock file     endpoints            .part → final     metadata KV    │
└──────────────────────────────────────────────────────────────────────┘
```

---

## 1. Authentication Flow

```
src/auth/mod.rs :: authenticate()
│
├─ Session::new()
│  ├─ Load cookies from ~/.config/kei/cookies/{user}
│  ├─ Load session data from {user}.session
│  └─ Acquire exclusive lock file ({user}.lock)
│
├─ validate_session()  ─── session_token exists? ──┐
│                                                  │
│  ┌────── YES: token valid ◀──────────────────────┘
│  │
│  └────── NO: token absent/expired
│          │
│          ▼
├─ srp::authenticate_srp()
│  └─ SRP-6a handshake with Apple auth servers
│
├─ twofa::authenticate_with_token()
│  └─ Exchange session token for account data
│
├─ check_requires_2fa()
│  │
│  ├─ YES: HSAv2 required
│  │  ├─ Interactive: trigger push, prompt for 6-digit code
│  │  ├─ submit-code: use provided code (no push)
│  │  ├─ get-code: trigger push only
│  │  └─ Headless: return AuthError::TwoFactorRequired (no push)
│  │
│  └─ NO: proceed
│
├─ twofa::trust_session()
│
└─ Return AuthResult { session, data }

Persistence:
  ~/.config/kei/cookies/{user}          JSON cookie jar
  ~/.config/kei/cookies/{user}.session  JSON session data
  ~/.config/kei/cookies/{user}.lock     Exclusive lock (prevents concurrent runs)
```

---

## 2. Photos API Layer

```
src/icloud/photos/mod.rs :: PhotosService
│
├─ service_root: "https://p{N}-ckdatabasews.icloud.com"
├─ session: Box<dyn PhotosSession>  (shared reqwest::Client via Arc)
├─ params: Arc<HashMap>  {remapEnums: true, getCurrentSyncToken: true}
│
├─ primary_library: PhotoLibrary  (zone: "PrimarySync")
├─ private_libraries: lazy HashMap<String, PhotoLibrary>
└─ shared_libraries: lazy HashMap<String, PhotoLibrary>

PhotoLibrary (src/icloud/photos/library.rs)
├─ service_endpoint: "{root}/database/1/com.apple.photos.cloud/production/{type}"
├─ zone_id: {"zoneName": "PrimarySync"}
└─ Implements Clone (Arc for params, clone_box for session)

PhotoAlbum (src/icloud/photos/album.rs)
├─ photo_stream()            Full enumeration (parallel fetchers)
├─ photo_stream_with_token() Full enumeration + syncToken capture
├─ changes_stream()          Incremental delta (sequential pages)
└─ len()                     Count via records/query
```

### API Endpoints Used

```
POST /records/query           Full photo enumeration (paginated)
POST /changes/database        Cheap pre-check: which zones changed?
POST /changes/zone            Incremental delta stream (token-chained)
POST /zones/list              Discover private/shared libraries
POST /assets/batch_get        Download URL resolution (CDN URLs)
GET  {cdn_url}                File download (supports Range headers)
```

---

## 3. Sync Mode Decision

```
src/main.rs (lines ~1295-1330)

                    ┌───────────────────┐
                    │ --no-incremental  │──── YES ──▶ SyncMode::Full
                    │ --reset-sync-token│
                    └────────┬──────────┘
                             │ NO
                             ▼
              ┌──────────────────────────┐
              │ Load sync_token:{zone}   │
              │ from metadata table      │
              └────────────┬─────────────┘
                           │
                ┌──── token found? ────┐
                │                      │
               YES                     NO
                │                      │
                ▼                      ▼
    SyncMode::Incremental      SyncMode::Full
    { zone_sync_token }
```

---

## 4. Watch Mode Pre-Check

```
src/main.rs (lines ~1226-1280)

Only in watch mode + incremental sync:

  ┌─────────────────────────────────────┐
  │ Load db_sync_token from metadata    │
  └──────────────┬──────────────────────┘
                 │
                 ▼
  ┌─────────────────────────────────────┐
  │ POST changes/database               │
  │ Body: {} or {"syncToken": "..."}    │
  └──────────────┬──────────────────────┘
                 │
                 ▼
  ┌─────────────────────────────────────┐
  │ Store new db_sync_token             │
  └──────────────┬──────────────────────┘
                 │
       ┌── zones empty? ──┐
       │                   │
      YES                  NO
       │                   │
       ▼                   ▼
  Skip cycle          Log changed zones
  (0 API calls)       Proceed to download
```

This is the **cheapest possible no-op check** — a single API call that returns immediately when nothing has changed. Drops a 7,300-photo library from ~75 API calls to 1.

---

## 5. Download Pipeline

```
src/download/mod.rs :: download_photos_with_sync()
│
├─── SyncMode::Full ──────────────────────────────────────────────────┐
│                                                                     │
│  download_photos_full_with_token()                                  │
│  │                                                                  │
│  ├─ photo_stream_with_token()                                       │
│  │  └─ Parallel fetchers (N = concurrent_downloads)                 │
│  │     Each fetcher spans an offset range                           │
│  │     Channel buffer: page_size × num_fetchers                     │
│  │     syncToken captured from final page (moreComing: false)       │
│  │                                                                  │
│  └─ stream_and_download_from_stream()  ──────────────────▶ [Phase 1]│
│                                                                     │
├─── SyncMode::Incremental ───────────────────────────────────────────┐
│                                                                     │
│  download_photos_incremental()                                      │
│  │                                                                  │
│  ├─ changes_stream(zone_sync_token)                                 │
│  │  └─ Sequential pages (token chaining, no parallelism)            │
│  │     Page N's syncToken → Page N+1's request body                 │
│  │     Yields ChangeEvent { record_name, reason, asset }            │
│  │     Cross-page CPLMaster/CPLAsset pairing via DeltaRecordBuffer  │
│  │                                                                  │
│  ├─ Filter: ChangeReason::Created → extract PhotoAssets             │
│  │  (Skip: SoftDeleted, HardDeleted, Hidden)                        │
│  │                                                                  │
│  └─ run_download_pass()  ────────────────────────────────▶ [Phase 1]│
│                                                                     │
│  On SyncTokenError (invalid/expired token):                         │
│  └─ Fallback: retry entire cycle with SyncMode::Full                │
└─────────────────────────────────────────────────────────────────────┘
```

### Phase 1: Streaming Download

```
stream_and_download_from_stream()

  ┌──────────────── Pre-load from DB ────────────────────┐
  │ downloaded_ids:  HashSet<(id, version_size)>         │
  │ downloaded_sums: HashMap<(id, vs), checksum>         │
  │ config_hash:     trust-state fingerprint             │
  │   If config_hash matches → skip filesystem checks    │
  │   If mismatch → verify all files (re-download stale) │
  └──────────────────────┬───────────────────────────────┘
                         │
                         ▼
  ┌─────────────────── Producer Task ─────────────────────┐
  │                                                       │
  │  for each PhotoAsset in stream:                       │
  │    ├─ Content filter (type, date range, media)        │
  │    ├─ Path resolution (folder structure template)     │
  │    ├─ Collision detection (case-insensitive)          │
  │    ├─ Early skip gate:                                │
  │    │   ├─ DB says downloaded + checksum matches → skip│
  │    │   ├─ DB says not downloaded → download           │
  │    │   └─ Unknown → check filesystem                  │
  │    ├─ upsert_seen() in DB (mark as pending)           │
  │    └─ Send DownloadTask to mpsc channel               │
  │                                                       │
  └──────────────────────┬────────────────────────────────┘
                         │ mpsc channel
                         ▼
  ┌──────────── Download Concurrency Pool ────────────────┐
  │  buffer_unordered(concurrent_downloads)               │
  │                                                       │
  │  Per DownloadTask:                                    │
  │    ├─ download_file()                                 │
  │    │   ├─ HTTP GET with Range header (resume .part)   │
  │    │   ├─ Write to {path}.part                        │
  │    │   ├─ SHA256 verify vs iCloud checksum            │
  │    │   │   (handles 32-byte raw + 33-byte prefixed)   │
  │    │   ├─ Move .part → final path                     │
  │    │   └─ Retry with exponential backoff on failure   │
  │    │                                                  │
  │    ├─ Set file mtime to photo created_date            │
  │    ├─ Optionally set EXIF DateTimeOriginal            │
  │    └─ Compute local SHA256 for DB storage             │
  │                                                       │
  │  Per-item DB updates (inline, after each download):   │
  │    ├─ mark_downloaded() on success                    │
  │    └─ mark_failed() on exhausted retries              │
  └───────────────────────────────────────────────────────┘
```

### Phase 2: Retry Failed Downloads

```
  For each failed task from Phase 1:
    ├─ Re-enumerate albums (fetch fresh CDN URLs)
    ├─ Rebuild DownloadTasks with new URLs
    ├─ Attempt re-download
    └─ Final retry with exponential backoff
```

---

## 6. SQLite State Management

```
src/state/schema.rs (version 4)
src/state/db.rs     (SqliteStateDb)

Database: ~/.config/kei/cookies/{user}.db  (WAL mode, NORMAL sync)

┌─────────────────────────── assets ───────────────────────────────────┐
│ PRIMARY KEY (id, version_size)                                       │
│                                                                      │
│ id               TEXT     CloudKit record name                       │
│ version_size     TEXT     Original / Medium / Thumb / Alternative    │
│ checksum         TEXT     iCloud-provided SHA256                     │
│ filename         TEXT     Original filename from iCloud              │
│ created_at       TEXT     Photo creation timestamp                   │
│ added_at         TEXT     Date added to iCloud                       │
│ size_bytes       INTEGER  File size in bytes                         │
│ media_type       TEXT     image / video                              │
│ status           TEXT     pending / downloaded / failed              │
│ downloaded_at    TEXT     When successfully downloaded               │
│ local_path       TEXT     Path where file was saved                  │
│ last_seen_at     TEXT     Last time seen in API response             │
│ download_attempts INTEGER Number of download attempts                │
│ last_error       TEXT     Last error message (if failed)             │
│ local_checksum   TEXT     SHA256 of the downloaded file              │
│ download_checksum TEXT    Checksum at time of download (v4)          │
└──────────────────────────────────────────────────────────────────────┘

┌─────────────────────── sync_runs ────────────────────────────────────┐
│ id               INTEGER  Auto-increment                             │
│ started_at       TEXT     Cycle start time                           │
│ completed_at     TEXT     Cycle end time                             │
│ total_seen       INTEGER  Assets seen in API                         │
│ downloaded       INTEGER  Successfully downloaded                    │
│ skipped          INTEGER  Already present / filtered                 │
│ failed           INTEGER  Download failures                          │
└──────────────────────────────────────────────────────────────────────┘

┌─────────────────────── metadata ─────────────────────────────────────┐
│ key              TEXT     PRIMARY KEY                                │
│ value            TEXT                                                │
│                                                                      │
│ Reserved keys:                                                       │
│   db_sync_token           Database-level token (changes/database)    │
│   sync_token:{zone}       Zone-level token (changes/zone)            │
│   config_hash             DownloadConfig fingerprint (trust-state)   │
└──────────────────────────────────────────────────────────────────────┘
```

### DB Access Patterns

```
Per-sync pre-load (O(1) lookups during stream):
  get_downloaded_ids()       → HashSet<(id, version_size)>
  get_downloaded_checksums() → HashMap<(id, vs), checksum>

Per-asset (during stream):
  upsert_seen()              Mark asset as pending (insert or update)

Per-download (inline, after each item):
  mark_downloaded()          Status → downloaded, store local_path + checksum
  mark_failed()              Status → failed, store error message

Token management:
  get_metadata(key)          Load stored sync token
  set_metadata(key, value)   Persist new sync token
```

---

## 7. SyncToken Lifecycle

```
┌─────────────────── First Run (Full Sync) ───────────────────────────┐
│                                                                     │
│  1. No stored token → SyncMode::Full                                │
│  2. photo_stream_with_token() enumerates all assets                 │
│  3. syncToken captured from final query response page               │
│  4. After successful download cycle:                                │
│     ├─ set_metadata("sync_token:{zone}", token)                     │
│     └─ set_metadata("db_sync_token", db_token)  [if watch mode]     │
│                                                                     │
└─────────────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌─────────────────── Subsequent Runs (Incremental) ───────────────────┐
│                                                                     │
│  1. Load sync_token:{zone} from metadata → SyncMode::Incremental    │
│                                                                     │
│  2. [Watch mode only] changes/database pre-check:                   │
│     ├─ POST with db_sync_token                                      │
│     ├─ zones empty? → skip cycle (0 additional API calls)           │
│     └─ Store new db_sync_token                                      │
│                                                                     │
│  3. changes/zone with zone_sync_token:                              │
│     ├─ Page 1: {syncToken: "tok-A"} → records + {syncToken: "tok-B"}│
│     ├─ Page 2: {syncToken: "tok-B"} → records + {syncToken: "tok-C"}│
│     ├─ ...until moreComing: false                                   │
│     └─ Final token: "tok-N"                                         │
│                                                                     │
│  4. Process ChangeEvents, download new/modified assets              │
│                                                                     │
│  5. Store "tok-N" as new sync_token:{zone}                          │
│                                                                     │
└─────────────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌─────────────────── Error / Fallback ────────────────────────────────┐
│                                                                     │
│  SyncTokenError::InvalidToken                                       │
│  ├─ Detected via changes/zone returning BAD_REQUEST                 │
│  ├─ check_changes_zone_error() → SyncTokenError                     │
│  ├─ Propagated as anyhow::Error, downcast in download pipeline      │
│  └─ Fallback: clear stored token, retry with SyncMode::Full         │
│                                                                     │
│  SyncTokenError::ZoneNotFound                                       │
│  └─ Zone was deleted (shared library removed) → skip zone           │
│                                                                     │
│  --reset-sync-token CLI flag                                        │
│  └─ Clears db_sync_token + sync_token:{zone} → forces Full sync     │
│                                                                     │
└─────────────────────────────────────────────────────────────────────┘
```

### Token Persistence Rules

| Scenario | Action |
|----------|--------|
| Successful full sync | Store zone token from final query page |
| Successful incremental sync | Store zone token from final changes/zone page |
| Partial failure (some downloads failed) | Still advance token (delta was fetched; retries via state DB) |
| Session expired mid-sync | Token remains valid (session-independent); store intermediate token |
| Crash mid-sync | Each page's token is a valid resume point (crash-safe) |
| Invalid/expired token | Clear stored token, fall back to full enumeration |

---

## 8. Error Handling

```
┌──────────── Download Errors ────────────────────────────────────────┐
│                                                                     │
│  DownloadError (src/download/error.rs)                              │
│  ├─ is_retryable():  429, 5xx → true; 4xx → false; disk → false     │
│  ├─ is_session_expired():  401, 403 → true                          │
│  └─ Retry: exponential backoff (base 2s, max 60s)                   │
│                                                                     │
│  Session Expiry:                                                    │
│  ├─ DownloadOutcome::SessionExpired bubbles up to main loop         │
│  ├─ attempt_reauth() called (max 3 attempts, 30s timeout)           │
│  └─ Watch mode: re-auth + continue; single-shot: bail               │
│                                                                     │
│  SyncTokenError:                                                    │
│  ├─ InvalidToken: changes/zone returned BAD_REQUEST                 │
│  │   └─ Automatic fallback to SyncMode::Full                        │
│  └─ ZoneNotFound: ZONE_NOT_FOUND in response                        │
│      └─ Zone deleted (shared library removed)                       │
│                                                                     │
│  CloudKitServerError (src/icloud/photos/error.rs)                   │
│  └─ Retried with exponential backoff via retry_post()               │
│                                                                     │
└─────────────────────────────────────────────────────────────────────┘
```

---

## 9. Graceful Shutdown

```
src/shutdown.rs

  Signals: SIGINT (Ctrl+C), SIGTERM, SIGHUP
  │
  ├─ First signal:
  │  ├─ Cancel CancellationToken
  │  ├─ Producer stops streaming from API
  │  ├─ In-flight downloads drain naturally
  │  └─ Watch cycle breaks out of sleep
  │
  └─ Second signal:
     └─ Force exit (code 130)

  Integration points:
  ├─ Producer task checks shutdown_token.is_cancelled()
  ├─ Consumer tasks check between downloads
  └─ Watch loop: tokio::select! { sleep, cancelled() }
```

---

## 10. Complete Sync Cycle (End-to-End)

```
┌──────────┐     ┌───────────┐     ┌──────────────┐     ┌──────────┐
│   CLI    │────▶│   Auth    │────▶│ PhotosService│────▶│  State   │
│  Config  │     │  Session  │     │   (iCloud)   │     │   DB     │
└──────────┘     └───────────┘     └──────────────┘     └──────────┘
                                          │                    │
                                          ▼                    ▼
                              ┌──────────────────────────────────────┐
                              │          Main Sync Loop              │
                              │                                      │
                              │  for each cycle:                     │
                              │                                      │
                              │  1. Load sync tokens from DB         │
                              │     ┌──────┐    ┌─────────────┐      │
                              │     │ Full │    │ Incremental │      │
                              │     └──┬───┘    └──────┬──────┘      │
                              │        │               │             │
                              │  2. [Watch] changes/database check   │
                              │        │               │             │
                              │  3. Stream photos from API           │
                              │     (parallel)    (sequential)       │
                              │        │               │             │
                              │  4. Filter + build DownloadTasks     │
                              │        │               │             │
                              │  5. Download concurrently            │
                              │     ├─ HTTP GET + .part files        │
                              │     ├─ SHA256 verification           │
                              │     ├─ EXIF / mtime updates          │
                              │     └─ Batch DB updates              │
                              │        │               │             │
                              │  6. Retry failed with fresh URLs     │
                              │        │               │             │
                              │  7. Store sync tokens in DB          │
                              │        │               │             │
                              │  8. [Watch] sleep → next cycle       │
                              │     [Single] exit                    │
                              │                                      │
                              └──────────────────────────────────────┘
```
