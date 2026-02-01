# Changelog

## Current (unreleased)

### Authentication
- SRP-6a authentication with Apple's custom protocol variants
- Two-factor authentication (trusted device code) with trust persistence
- Session persistence with cookie management, lock files, and proactive token refresh

> [!IMPORTANT]
> **Change from Python:** Lock files prevent concurrent instances from corrupting session state; trust token age is tracked with warnings before expiry; expired cookies are pruned on load

### Downloads
- Streaming download pipeline with configurable concurrent downloads (`--threads-num`)
- Resumable partial downloads via HTTP Range requests with SHA256 verification
- Retry with exponential backoff and transient/permanent error classification (`--max-retries`, `--retry-delay`)
- Progress bar tracking download progress, auto-hidden in non-TTY environments (`--no-progress-bar`)
- Two-phase cleanup pass — retries failures with fresh CDN URLs
- Low memory streaming for large libraries (100k+ photos)

> [!IMPORTANT]
> **Change from Python:** Downloads begin as soon as the first API page returns, rather than enumerating the entire library before starting — eliminates multi-minute startup delays on large libraries

> [!TIP]
> **Change from Python:** `PhotoAsset` no longer retains raw JSON blobs; version URLs are pre-parsed at construction, reducing per-asset memory

> [!IMPORTANT]
> **Change from Python:** Partial `.part` files are resumed via HTTP Range; existing bytes are hashed on resume so the final SHA256 checksum covers the entire file

> [!TIP]
> **Change from Python:** Failed downloads get a cleanup pass that re-fetches URLs from iCloud before retrying, fixing expired CDN URL failures on large syncs

> [!NOTE]
> **Change from Python:** API calls (album fetch, zone list) retry automatically on 5xx/429 errors

> [!NOTE]
> **Change from Python:** Album photo fetching runs concurrently (bounded by `--threads-num`) instead of sequentially

### Photos & Media
- Photo, video, and live photo MOV downloads with size variants
- RAW file alignment (`--align-raw`: as-is, original, alternative)
- Live photo MOV filename policies (suffix, original)
- Content filtering by media type, date range, album, and recency

> [!TIP]
> **Change from Python:** Live photo MOV size is independently configurable (`--live-photo-size`)

### Organization
- Date-based folder structures (`--folder-structure`)
- Filename sanitization and deduplication policies
- EXIF date tag read/write (`DateTime`, `DateTimeOriginal`, `DateTimeDigitized`) and file modification time sync

### Operational
- Dry-run, auth-only, list albums/libraries modes
- Watch mode with session validation between cycles
- Album and shared library enumeration
- Log level control, domain selection (com/cn), custom cookie directory

> [!TIP]
> **Change from Python:** `--recent N` stops fetching from the API after N photos instead of enumerating the entire library first

> [!CAUTION]
> **Change from Python:** `--until-found` removed — will be replaced by stateful incremental sync with local database
