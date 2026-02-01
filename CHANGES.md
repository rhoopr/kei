# Behavioral Changes from Python icloud-photos-downloader

- **Robust session persistence** — lock files prevent concurrent instances from corrupting session state; trust token age is tracked with warnings before expiry; expired cookies are pruned on load; session is validated and refreshed in watch mode between cycles; `SharedSession` (Arc+RwLock) threads through the download layer for future mid-sync re-auth

- **Streaming download pipeline** — downloads begin as soon as the first API page returns, rather than enumerating the entire library before starting. For large libraries (100k+ photos) this eliminates multi-minute startup delays
- **Compact asset representation** — `PhotoAsset` no longer retains raw JSON blobs; version URLs are pre-parsed at construction, reducing per-asset memory
- `--recent N` stops fetching from the API after N photos instead of enumerating the entire library first
- `--until-found` removed — will be replaced by stateful incremental sync with local database
- Album photo fetching runs concurrently (bounded by `--threads-num`) instead of sequentially
- Downloads retry with exponential backoff on transient errors (default: 2 retries); configurable via `--max-retries` and `--retry-delay`
- Failed downloads get a cleanup pass that re-fetches URLs from iCloud before retrying, fixing expired CDN URL failures on large files
- Partial `.part` files are resumed via HTTP Range requests; existing bytes are hashed on resume so the final SHA256 checksum still covers the entire file
- API calls (album fetch, zone list) retry automatically on 5xx/429 errors
- Live photo MOV files are downloaded alongside photos with configurable size (`--live-photo-size`) and filename policy (`--live-photo-mov-filename-policy`)
