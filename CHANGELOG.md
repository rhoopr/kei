# Changelog

## Performance vs Python icloudpd

Benchmarked against Python icloudpd 1.32.2 on macOS with WiFi (84 runs, ~500GB total downloaded):

| Photo Count | Python 1T | Rust 1T | Rust 5T | Rust 10T |
|-------------|-----------|---------|---------|----------|
| 50 photos | 54s | 46s (1.2x) | 27s (2.0x) | 25s (2.1x) |
| 500 photos | 5m 8s | 4m 36s (1.1x) | 1m 43s (3.0x) | 1m 43s (3.0x) |
| 5000 photos | 96 min | 79 min (1.2x) | 30 min (3.2x) | **25 min (3.8x)** |
| Memory | 63-77 MB | 24-41 MB | 25-30 MB | 27-35 MB |

**Key Takeaways:**
- **60-65% less memory** than Python across all test sizes
- **3-4x faster** with concurrent downloads (5-10 threads)
- Single-threaded Rust is 10-20% faster due to lower runtime overhead
- At 5000 photos (144GB): Python takes 96 min, Rust 10T takes 25 min

---

## Current (unreleased)

### Authentication
- SRP-6a authentication with Apple's custom protocol variants (including automatic `s2k`/`s2k_fo` negotiation)
- Two-factor authentication (trusted device code) with trust token persistence
- Session persistence with cookie management and lock files
- Interactive secure password prompt when `--password` is not provided
- Automatic SRP repair flow on HTTP 412 responses
- Domain redirect detection — if Apple indicates a region-specific domain (e.g. `.cn`), the user is prompted to re-run with `--domain`

> [!IMPORTANT]
> **Change from Python:** Lock files prevent concurrent instances from corrupting session state; expired cookies are pruned on load

> [!NOTE]
> **Change from Python:** Cookie files support both the new JSON format and the legacy Python icloudpd tab-separated format for migration

> [!TIP]
> **Change from Python:** Session and cookie files are restricted to owner-only permissions (`0600`) on Unix

### Downloads
- Streaming download pipeline with configurable concurrent downloads (`--threads-num`, default: 10)
- Separate HTTP client for downloads — no total request timeout, so large files aren't killed mid-transfer. Uses 30s connect timeout and 120s read timeout for stall detection.

> [!IMPORTANT]
> **Change from Python:** `--threads-num` controls actual concurrent downloads (default: 10) — Python deprecated this flag and always downloads sequentially
- Resumable partial downloads via HTTP Range requests with SHA256 verification (256KB hash buffer for fast resume)
- Retry with exponential backoff, jitter, and transient/permanent error classification (`--max-retries` default: 3, `--retry-delay`)

> [!TIP]
> **Change from Python:** `--max-retries` and `--retry-delay` are new flags — Python hardcodes `MAX_RETRIES = 0` with no user control
- Progress bar tracking download progress, auto-hidden in non-TTY environments (`--no-progress-bar`). Skipped files (already downloaded) advance the counter on resume.
- Live photo MOV collision detection — when a regular video occupies the same filename, the companion MOV is saved with an asset ID suffix (e.g. `IMG_0001-ASSET_ID.MOV`)
- File collision deduplication (`--file-match-policy`) — when multiple iCloud assets share the same filename but have different content, the default `name-size-dedup-with-suffix` policy saves both files by appending the file size (e.g. `photo.jpg` and `photo-12345.jpg`)
- Two-phase cleanup pass — retries failures with fresh CDN URLs
- Concurrent downloads with collision detection (tasks are buffered in memory for deduplication)
- Deterministic `.part` filenames derived from checksum (base32-encoded, filesystem-safe)

> [!IMPORTANT]
> **Change from Python:** Downloads begin as soon as the first API page returns, rather than enumerating the entire library before starting — eliminates multi-minute startup delays on large libraries

> [!IMPORTANT]
> **Change from Python:** Partial `.part` files are resumed via HTTP Range; existing bytes are hashed on resume so the final SHA256 checksum covers the entire file

> [!TIP]
> **Change from Python:** Failed downloads get a cleanup pass that re-fetches URLs from iCloud before retrying, fixing expired CDN URL failures on large syncs

> [!TIP]
> **Change from Python:** `PhotoAsset` no longer retains raw JSON blobs; version URLs are pre-parsed at construction, reducing per-asset memory and making `versions()` infallible

> [!NOTE]
> **Change from Python:** API calls (album fetch, zone list) retry automatically on 5xx/429 errors with jitter to prevent thundering herd

> [!NOTE]
> **Change from Python:** Album photo fetching runs concurrently (bounded by `--threads-num`) instead of sequentially

> [!NOTE]
> **Change from Python:** Error classification distinguishes retryable errors (5xx, 429 rate limit, checksum mismatch from truncated transfer) from permanent errors (4xx, disk errors), avoiding wasted retries

### Photos & Media
- Photo, video, and live photo MOV downloads with size variants
- RAW file alignment (`--align-raw`: as-is, original, alternative)
- Live photo MOV filename policies (suffix, original)
- Content filtering by media type, date range, album, and recency
- Smart album support (time-lapse, videos, slo-mo, bursts, favorites)
- Handles both plain-text and base64-encoded (`ENCRYPTED_BYTES`) filenames from CloudKit
- Asset type detection via CloudKit `itemType` with filename extension fallback

> [!TIP]
> **Change from Python:** Live photo MOV size is independently configurable (`--live-photo-size`)

### Organization
- Date-based folder structures (`--folder-structure`)
- Filename sanitization (strips `/\:*?"<>|`) and deduplication policies
- EXIF date tag read/write (`DateTime`, `DateTimeOriginal`, `DateTimeDigitized`) and file modification time sync

> [!NOTE]
> **Change from Python:** Folder structure format accepts both Python-style `{:%Y}` and plain `%Y` strftime syntax for backwards compatibility

### Operational
- Dry-run, auth-only, list albums/libraries modes
- Watch mode with automatic session validation and re-authentication between cycles
- **Mid-sync session recovery** — if Apple invalidates the session during a large download, automatically re-authenticates and resumes (up to 3 attempts). In headless mode, provides actionable guidance to run `--auth-only` interactively for 2FA.

> [!TIP]
> **Change from Python:** Python icloudpd exits with auth errors if the session expires mid-sync; icloudpd-rs detects 401/403 responses and triggers re-authentication automatically

- Graceful shutdown — first Ctrl+C / SIGTERM / SIGHUP finishes in-flight downloads then exits; second signal force-exits immediately. Partial `.part` files are kept for smart resume on next run. Watch mode sleep is interruptible.
- Library indexing readiness check before querying (waits for CloudKit indexing to finish)
- Album and shared library enumeration
- Log level control (`--log-level`: `debug`, `info`, `warn`, `error`; default: `info`), domain selection (com/cn), custom cookie directory

> [!TIP]
> **Change from Python:** `--recent N` stops fetching from the API after N photos instead of enumerating the entire library first

> [!CAUTION]
> **Change from Python:** `--until-found` removed — will be replaced by stateful incremental sync with local database

### Not Yet Wired (parsed but inactive)
- `--force-size` — download only the requested size without fallback to original
- `--keep-unicode-in-filenames` — preserve Unicode characters in filenames
- `--only-print-filenames` — print download paths without downloading
- `--library` — select which library to download from (default: PrimarySync)
