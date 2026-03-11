# CloudKit syncToken — Complete Specification

> **Note:** All tokens, record names, user IDs, and UUIDs in this document are sanitized examples. Real values have been replaced with structurally similar placeholders.

Empirically derived from live testing against Apple's private iCloud Photos CloudKit API.
All findings verified against a real iCloud Photos library.

---

## What Is a syncToken?

An opaque, base64-like string (~48 characters) that acts as a **bookmark into iCloud's change history log**. It represents a zone-level snapshot — a point-in-time marker in the zone's mutation stream. Every record creation, modification, or deletion advances the stream; a syncToken lets you resume reading from any previous position.

Tokens are **not** page cursors (those are `continuationMarker`). They are **not** tied to a session, user agent, or query offset. They are zone-wide invariants that encode only "where in the history log am I?"

Example token: `AaBbCcDdEeFf0123456789GgHhIiJjKkLlMmNn...`

---

## Endpoints

All endpoints live under:
```
{ckdatabasews_url}/database/1/com.apple.photos.cloud/production/{library_type}
```

Where `ckdatabasews_url` comes from `accountLogin` response at `.webservices.ckdatabasews.url`, and `library_type` is `private` or `shared`.

All requests include query parameters:
```
?clientId={url_encoded_client_id}&getCurrentSyncToken=true&remapEnums=true
```

All requests use:
- Method: POST
- Header: `Content-type: text/plain`
- Header: `Origin: https://www.icloud.com`

### Endpoint Summary

| Endpoint | Purpose | syncToken | Notes |
|----------|---------|-----------|-------|
| `/records/query` | Query by index (current approach) | Output only (in response) | What we use today for photo enumeration |
| `/changes/database` | Database-level pre-check | Input + output | Returns which zones have changes; cheapest call |
| `/changes/zone` | Zone-level record deltas | Input + output | Returns actual changed records; the workhorse |
| `/records/changes` | Deprecated zone deltas | Input + output | Still works; same data as changes/zone |
| `/internal/records/query/batch` | Batch count queries | None | Used by `album.len()` for HyperionIndexCountLookup |
| `/zones/list` | List available zones | None | Discovers PrimarySync + SharedSync-{UUID} zone names |

---

## API Details

### `/records/query` (existing — what we use today)

Request:
```json
{
    "query": {
        "filterBy": [
            {
                "fieldName": "startRank",
                "fieldValue": {"type": "INT64", "value": 0},
                "comparator": "EQUALS"
            },
            {
                "fieldName": "direction",
                "fieldValue": {"type": "STRING", "value": "ASCENDING"},
                "comparator": "EQUALS"
            }
        ],
        "recordType": "CPLAssetAndMasterByAssetDateWithoutHiddenOrDeleted"
    },
    "resultsLimit": 400,
    "desiredKeys": ["filenameEnc", "itemType", "addedDate", ...],
    "zoneID": {"zoneName": "PrimarySync"}
}
```

Response:
```json
{
    "records": [ ... ],
    "syncToken": "AaBbCcDdEeFf0123...",
    "continuationMarker": "AbCdEfGh..."
}
```

**Key facts:**
- `syncToken` is returned at the top level alongside `records`
- **Our `QueryResponse` struct captures `syncToken`** via the `sync_token: Option<String>` field
- `getCurrentSyncToken=true` query param is required to get the token; without it, no token
- We already send `getCurrentSyncToken=true` (set in `PhotosService::new()` at mod.rs:56)
- Token is zone-level: same value regardless of `startRank` offset or `resultsLimit`
- `continuationMarker` is separate from `syncToken` — it's a page cursor, not a change bookmark

### `/changes/database` — Zone-Level Pre-Check

The cheapest possible call. Answers: "has anything changed in any zone?"

Request (first call, no token):
```json
{}
```

Response (zones have changes):
```json
{
    "syncToken": "db_level_token...",
    "moreComing": false,
    "zones": [
        {
            "zoneID": {"zoneName": "PrimarySync"},
            "syncToken": "zone_level_token..."
        }
    ]
}
```

Request (subsequent call with token):
```json
{
    "syncToken": "db_level_token..."
}
```

Response (no changes since last call):
```json
{
    "syncToken": "db_level_token...",
    "moreComing": false,
    "zones": []
}
```

**Key facts:**
- Returns 0 zones when nothing changed — **zero-cost no-op check**
- Has its own database-level syncToken (separate from zone tokens)
- Each zone in the response has its own syncToken
- Use this as the first call in watch mode: if zones is empty, skip everything

### `/changes/zone` — Record-Level Deltas (the workhorse)

Returns actual record changes since a given syncToken.

Request (first call, no token — enumerates entire history):
```json
{
    "zones": [{
        "zoneID": {"zoneName": "PrimarySync"},
        "resultsLimit": 200
    }]
}
```

Request (subsequent call with token):
```json
{
    "zones": [{
        "zoneID": {"zoneName": "PrimarySync"},
        "syncToken": "AaBbCcDdEeFf0123...",
        "resultsLimit": 200
    }]
}
```

Response:
```json
{
    "zones": [{
        "zoneID": {"zoneName": "PrimarySync"},
        "syncToken": "new_token_after_this_page...",
        "moreComing": true,
        "records": [
            {
                "recordName": "ABC123",
                "recordType": "CPLAsset",
                "fields": { ... },
                "recordChangeTag": "tag_abc"
            },
            {
                "recordName": "DEF456",
                "recordType": null,
                "deleted": true,
                "recordChangeTag": "tag_def"
            }
        ]
    }]
}
```

**Key facts:**
- `moreComing: true` means call again with the new `syncToken` to get more
- Page until `moreComing: false` to reach "caught up" state
- Caught-up state returns 0 records, `moreComing: false`
- Caught-up token is stable: repeated calls return 0 records (deterministic)
- Maximum effective page size is 200 records regardless of `resultsLimit`
- `desiredRecordTypes` filter is **unreliable** — must filter client-side
- `desiredKeys` filter works correctly
- Deletion has **two levels** — see "Deleted Records" section below
- `recordChangeTag` is present on all records including deleted ones

---

## Token Properties (All Empirically Verified)

### 1. Zone-Level Invariant

Token represents the entire zone's state, not a page position.

**Proof:** Querying `records/query` with `startRank=0` and `startRank=10` returns the same syncToken.

### 2. Deterministic

Same token, same call → same results. Every time.

**Proof:** Called the same caught-up token 5 times rapidly. All 5 returned 0 records with identical response tokens.

### 3. Idempotent

Using a token does not consume or invalidate it. You can re-use the same token indefinitely.

**Proof:** Token from page 3's response was called twice. Both times returned identical records, identical counts, identical next-tokens.

### 4. Random Access (Forward and Backward)

Tokens support jumping to any position in the history — no sequential requirement.

**Proof — Forward skip:** Saved token from page 5, then jumped directly to it (skipping pages 3-4). Returned exactly the same data as the original sequential pass through page 5.

**Proof — Backward jump:** After reaching page 10, used token from page 2. Returned exactly the same data as the original page 3 forward pass. Record names and counts matched perfectly.

### 5. Session-Independent

Tokens survive authentication refresh and can be stored persistently.

**Proof:** Saved a token, ran `cargo run -- sync --auth-only` to get new session cookies, rebuilt curl jar with new cookies, called the old token. Same records returned. No "token expired" error.

### 6. Cross-Endpoint Compatible

The syncToken from `records/query` is the SAME token format as `changes/zone` tokens. They are interchangeable.

**Proof:** Took a syncToken from a `records/query` response and passed it to `changes/zone`. Accepted without error, returned valid change records.

**Implication:** No "bootstrap" needed. The very first `records/query` scan already produces a usable syncToken for future `changes/zone` calls. Zero additional cost to start tracking.

### 7. Page-Size Independent

Changing `resultsLimit` doesn't affect record ordering or token meaning — just how many records you get per call.

**Proof:** Called the same token with `resultsLimit=50` and `resultsLimit=100`. First 50 records were identical in both responses.

### 8. Crash-Safe

Because tokens are deterministic and replayable, a crash at any point during pagination is recoverable. Store the last token received, restart from there.

---

## Record Types in changes/zone

Full enumeration of a ~7,300 photo library produced these record types across 42,787 total records:

| Record Type | Description |
|-------------|-------------|
| `CPLAsset` | Photo/video metadata (filename, dates, item type, album membership) |
| `CPLMaster` | Binary file reference (download URL, checksum, dimensions, file type) |
| `CPLAlbum` | Album metadata (appears on create AND modify — e.g., adding a photo updates the album record) |
| `CPLContainerRelation` | Album-to-asset membership link (1 record per photo-in-album relationship) |
| `CPLFaceCrop` | Face detection crop data |
| `CPLLibraryInfo` | Library-level settings/metadata (updated on every change — contains library-wide counters) |
| `CPLMemory` | "Memories" feature data |
| `CPLPerson` | People identification data |
| `CPLSuggestion` | Sharing suggestions |
| `null` | **Hard-deleted (purged)** — recordType is null, `record.deleted: true`. Soft deletes keep their original recordType. |

For photo sync, we only care about `CPLAsset` and `CPLMaster`. Everything else should be filtered client-side (since `desiredRecordTypes` is unreliable on changes/zone).

---

## Deleted Records

There are **two distinct levels of deletion**, and they look completely different in the API response.

### Level 1: Soft Delete (Moved to Trash)

When a user deletes a photo, it moves to "Recently Deleted" (30-day trash). The record is **modified**, not removed. Both the CPLAsset and CPLMaster records appear in the `changes/zone` delta as normal records with `deleted: false` but with `fields.isDeleted.value == 1`.

```json
{
    "recordName": "AExmplRecordName1abcdefghijk",
    "recordType": "CPLMaster",
    "deleted": false,
    "fields": {
        "isDeleted": {"value": 1, "type": "INT64"},
        "filenameEnc": {"value": "SU1HXzAyMDYuRE5H", "type": "STRING"},
        "dateExpunged": {"value": null},
        "resOriginalRes": { ... },
        ...all other fields still present...
    },
    "recordChangeTag": "tag_abc"
}
```

**Key observations:**
- `record.deleted` is `false` — the record is NOT purged
- `record.recordType` is still `"CPLMaster"` or `"CPLAsset"` — you CAN identify the type
- `record.fields` is fully populated — all metadata, download URLs, etc. still present
- `fields.isDeleted.value == 1` is the signal — this is a soft delete
- `fields.isExpunged.value == 0` — not yet permanently removed
- `fields.dateExpunged` — null until the 30-day window expires
- `fields.trashReason` — present on CPLAsset records (reason for deletion)
- The `filenameEnc` is on CPLMaster; CPLAsset has `filenameEnc: null` (as always)
- Both CPLMaster and CPLAsset records get `isDeleted: 1` in a soft delete

**This is the common case.** Most user-initiated deletions produce soft deletes. A single photo deletion generates 2 delta records (1 CPLMaster + 1 CPLAsset, both with `isDeleted: 1`).

### Level 2: Hard Delete (Purged / Expunged)

After the 30-day trash window (or manual "Delete All" from Recently Deleted), records are permanently removed from the zone. These appear in the `changes/zone` delta with `deleted: true` at the record level.

```json
{
    "recordName": "GHI789",
    "recordType": null,
    "deleted": true,
    "recordChangeTag": "tag_def"
}
```

**Key observations:**
- `record.deleted` is `true` — the record is purged from the zone
- `record.recordType` is `null` — you CANNOT tell if it was a CPLAsset or CPLMaster
- `record.fields` is absent or null — no metadata available
- `recordChangeTag` is still present
- These only appear in full history enumeration or after the 30-day expiry
- Apple retains extensive purge history: 15,618 hard-deleted records out of 42,787 total (36%) in a ~7,300 photo library

### Detection Logic

To correctly classify delta records:

```
if record.deleted == true:
    → HARD DELETE (purged, recordType unknown)
    Also used for CPLContainerRelation removal (album membership deleted)
elif record.fields.isDeleted.value == 1:
    → SOFT DELETE (trashed, full record available)
elif record.fields.isHidden.value == 1:
    → HIDDEN (moved to Hidden album, full record available)
elif record.fields.adjustmentType != null:
    → EDITED (non-destructive edit, CPLAsset only, CPLMaster unchanged)
elif record.fields.isFavorite.value == 1:
    → FAVORITED (CPLAsset only)
else:
    → NEW or MODIFIED
```

### Field Value Asymmetry (Important)

These boolean-like fields have **different "off" representations**:

| Field | Active/On | Inactive/Off | Notes |
|-------|-----------|-------------|-------|
| `isDeleted` | `1` | `null` (field absent) | Restored photos: `1` → `null` |
| `isHidden` | `1` | `0` | Un-hidden photos: `1` → `0` |
| `isFavorite` | `1` | `0` | Un-favorited: `1` → `0` |
| `isExpunged` | `1` | `0` or `null` | Permanent purge flag |

`isDeleted` is the outlier — it uses `null` for "not deleted" while `isHidden` and `isFavorite` use explicit `0`. Detection logic must handle both patterns.

### Verified in Live Testing

User performed: 5 deletions + 2 additions. Delta returned 16 records in 1 API call:
- 10 records with `fields.isDeleted == 1` (5 CPLMaster + 5 CPLAsset = 5 trashed photos)
- 5 records with no deletion flags (2 CPLMaster + 2 CPLAsset + 1 CPLLibraryInfo = 2 new files + metadata)
- 1 additional CPLAsset (associated with one of the new files)
- 0 records with `record.deleted == true`

The `record.deleted` field was `false` on ALL 16 records — soft deletes do NOT set this flag. Only checking `record.deleted` would miss every user-initiated deletion.

### "Recently Deleted" Album

Soft-deleted photos are also queryable via the "Recently Deleted" smart folder:
- Record type: `CPLAssetAndMasterDeletedByExpungedDate`
- Count via: `HyperionIndexCountLookup` with `indexCountID: "CPLAssetDeletedByExpungedDate"`
- This is a separate query path from `changes/zone` — useful for UI but not needed for incremental sync

---

## Error Handling

### Invalid/Garbage Token

Request with `syncToken: "INVALID_GARBAGE_TOKEN"`:

```json
{
    "zones": [{
        "zoneID": {"zoneName": "PrimarySync"},
        "serverErrorCode": "BAD_REQUEST",
        "reason": "Unknown sync continuation type"
    }]
}
```

- HTTP status: 200 (not an HTTP error)
- Error is in `.zones[0].serverErrorCode` and `.zones[0].reason`
- Recoverable: fall back to full enumeration (no token)

### Empty Token

`syncToken: ""` behaves like no token (full enumeration from the beginning).

---

## Pagination Mechanics

### changes/zone Pagination

```
CALL 1:  no token           → 200 records, moreComing=true,  token_1
CALL 2:  token_1            → 200 records, moreComing=true,  token_2
CALL 3:  token_2            → 200 records, moreComing=true,  token_3
...
CALL N:  token_{N-1}        → K records,   moreComing=false, token_N (caught-up)
CALL N+1: token_N           → 0 records,   moreComing=false, token_N (stable)
```

**Observed behavior with empty pages:** During full enumeration, the API sometimes returns pages with 0 records but `moreComing: true`. This is normal — keep calling. In one test, there were long stretches of empty pages (pages 27-96, all returning 0 records with `moreComing=true`) before records appeared again. This appears to be the API walking through internal log segments that have been compacted.

**Pagination token behavior (verified with resultsLimit=3):**
- Each page returns a **new, unique syncToken** — tokens advance with every page
- 17 records across 6 pages produced 7 unique tokens (initial + 1 per page)
- Records split cleanly across pages with no duplicates
- Intermediate tokens are fully functional — can be stored and resumed from if interrupted mid-pagination
- `moreComing=true` reliably signals more pages; final page has `moreComing=false`
- Record types are interleaved across pages (not grouped) — e.g., page 1 had CPLAlbum + CPLAsset + CPLContainerRelation

**Numbers from full enumeration of ~7,300 photo library:**
- Total change records: 42,787
- Total pages: ~450 (at resultsLimit=200, but many pages were empty)
- Effective records per non-empty page: ~200
- Deleted records: 15,618 (36% of total)
- Records with fields: ~27,169
- Records without fields (deleted/minimal): ~15,618

### records/query Pagination

Uses `continuationMarker` (not syncToken) for page-through:

```json
{
    "query": { ... },
    "continuationMarker": "marker_from_previous_response",
    "resultsLimit": 200
}
```

`continuationMarker` is a separate concept from `syncToken`. Both may appear in the same response:
- `syncToken`: zone-level change bookmark (stable across pages)
- `continuationMarker`: page cursor for this specific query (advances per page)

---

## Incremental Sync Architecture

### First Sync (Bootstrap)

The existing `records/query` flow works unchanged. The only modification needed:

1. Capture `syncToken` from the `records/query` response (currently dropped by `QueryResponse` struct)
2. Store it in the `metadata` SQLite table:
   ```sql
   INSERT OR REPLACE INTO metadata (key, value)
   VALUES ('sync_token_PrimarySync', 'AaBbCcDdEeFf0123...');
   ```

**Cost: zero additional API calls.** The token is already returned; we just need to stop dropping it.

### Subsequent Syncs (Incremental)

```
Step 1:  Load stored token from metadata table
Step 2:  POST /changes/database {syncToken: db_token}
         → If zones is empty: nothing changed, done
         → If PrimarySync in zones: proceed to Step 3
Step 3:  POST /changes/zone with stored zone token
         → Page through moreComing until caught-up
         → Filter for CPLAsset + CPLMaster records
         → Process additions/modifications/deletions
Step 4:  Store new caught-up token in metadata table
```

**Cost comparison:**

| Scenario | Full Scan (current) | Incremental (with syncToken) |
|----------|--------------------|-----------------------------|
| No changes | ~75 API calls (7,300 photos / 200 per page × 2 records each) | 1 API call (changes/database → 0 zones) |
| 1 photo added | ~75 API calls | 2 API calls (changes/database + changes/zone → 2 records) |
| 100 photos added | ~75 API calls | 2 API calls (changes/database + changes/zone → 200 records) |

### Watch Mode Optimization

Current watch mode re-scans the entire library on each interval. With syncToken:

```
Cycle 1: Full scan via records/query → capture token → store
Cycle 2: changes/database → if empty, sleep → if changed, changes/zone with token → process deltas → store new token
Cycle 3: changes/database → ...
```

A no-change cycle goes from ~75 API calls to **1 API call**. A small-change cycle goes from ~75 to **2**.

---

## Live Delta Detection Results

### Test 1: Baseline → Detect (2 API calls)

Established baseline via `records/query` syncToken (1 call), verified caught-up with `changes/zone` (1 call, 0 records). User then made 7 changes. Detection via `changes/database` (1 call) + `changes/zone` (1 call) = **2 API calls total**.

Delta: 16 records in a single `changes/zone` page.

**Trashed (5 files — `fields.isDeleted == 1`):**
- `IMG_0173.DNG` (CPLMaster + CPLAsset)
- `IMG_0174.DNG` (CPLMaster + CPLAsset)
- `IMG_0176.PNG` (CPLMaster + CPLAsset)
- `IMG_0206.DNG` (CPLMaster + CPLAsset)
- `IMG_0207.DNG` (CPLMaster + CPLAsset)

**Added (2 files — no deletion flags):**
- `IMG_0210.DNG` — photo, addedDate 2026-03-10 13:50:30 UTC
- `IMG_0211.MOV` — short video, addedDate 2026-03-10 13:50:41 UTC

**Metadata:**
- 1 `CPLLibraryInfo` update (library counts changed)

Each photo change produces **2 records**: one CPLMaster (binary reference with filename) and one CPLAsset (metadata with dates). So 7 user-visible changes = 14 photo records + 1 metadata + 1 extra CPLAsset = 16 total.

### Cost comparison for this test

| Approach | API Calls | Records Processed |
|----------|-----------|-------------------|
| Full scan (current) | ~75 | ~14,600 (7,300 photos × 2 records) |
| Incremental (syncToken) | **2** | **16** |

### Test 2: Photo Edit, Restore from Trash, Live Photo (2 API calls)

After test 1, user performed 3 operations. Detection: `changes/database` (1 call) + `changes/zone` (1 call) = **2 API calls**, 5 delta records.

#### Photo Edit (crop/filter in Photos app)

Only the **CPLAsset** is modified. The CPLMaster is untouched.

```json
{
    "recordType": "CPLAsset",
    "deleted": false,
    "fields": {
        "adjustmentType": {"value": "com.apple.photo"},
        "adjustmentCreatorCode": {"value": "com.apple.mobileslideshow"},
        "adjustmentRenderType": {"value": 19968},
        "adjustmentCompoundVersion": {"value": "1.9.1"},
        "customRenderedValue": {"value": 10},
        "adjustmentSimpleDataEnc": {"value": "...base64 edit parameters..."},
        "adjustedMediaMetaDataEnc": {"value": "...base64 EXIF for adjusted rendition..."},
        "adjustmentTimestampEnc": {"value": "...base64 timestamp..."},
        "adjustmentSourceType": {"value": 0},
        "otherAdjustmentsFingerprint": {"value": "AExmplOtherFingerprint1abcde"},
        "masterRef": {"value": {"recordName": "AExmplMasterRef1abcdefghijk"}},
        ...
    }
}
```

**Implications for sync:**
- The original file (CPLMaster) does NOT change — no re-download needed for the original
- The edit is non-destructive: stored as adjustment parameters on the CPLAsset
- To get the edited rendition, fetch `resJPEGFullRes` (Apple pre-renders the adjusted version)
- Detect edits via: `adjustmentType != null` on the CPLAsset
- `adjustmentCreatorCode` tells you which app made the edit (e.g., `com.apple.mobileslideshow` = Photos app)
- `customRenderedValue: 10` indicates the image has been processed/edited

#### Restore from Trash (Un-delete)

Only the **CPLAsset** is modified. The CPLMaster is untouched.

```json
{
    "recordType": "CPLAsset",
    "deleted": false,
    "fields": {
        "isDeleted": null,
        "isExpunged": null,
        "masterRef": {"value": {"recordName": "AExmplRestoredRef1abcdefghi"}},
        "addedDate": {"value": 1769125959633},
        ...
    }
}
```

**Key finding:** `isDeleted` goes from `1` back to `null` (not `0`). The field is simply absent/null when the photo is active.

**Implications for sync:**
- No new CPLMaster — the original file is still in iCloud, never deleted from storage
- Detect restore via: `isDeleted` is `null` on a CPLAsset whose `recordName` was previously tracked as deleted
- If the local file was already removed during a previous sync (because it was trashed), it needs to be re-downloaded
- The `masterRef` still points to the same CPLMaster — download URL is still valid

#### Live Photo

A Live Photo produces **1 CPLMaster** and **1 CPLAsset** — not two separate records. The video component is embedded in the CPLMaster's resource fields.

```json
{
    "recordType": "CPLMaster",
    "fields": {
        "filenameEnc": {"value": "SU1HXzAyMTIuSEVJQw=="},
        "itemType": {"value": "public.heic"},
        "resOriginalRes": {
            "value": {
                "fileChecksum": "...",
                "size": ...,
                "downloadURL": "https://cvws-h2.icloud-content.com/..."
            }
        },
        "resOriginalVidComplRes": {
            "value": {
                "fileChecksum": "AExmplChecksum1abcdefghijkl",
                "size": 4315264,
                "downloadURL": "https://cvws-h2.icloud-content.com/...",
                "wrappingKey": "AAAAAAAAAAAAAAAAAAAAAA=="
            }
        },
        "resOriginalVidComplFileType": {"value": "com.apple.quicktime-movie"},
        "resOriginalVidComplFileSize": {"value": 4315264},
        "resOriginalVidComplWidth": {"value": 1308},
        "resOriginalVidComplHeight": {"value": 1744},
        "resVidMedRes": { ... },
        "resVidSmallRes": { ... },
        "videoFrameRate": {"value": 0},
        ...
    }
}
```

**Key finding:** The video is NOT a separate file in CloudKit. It's a resource variant on the same CPLMaster record, accessed via `resOriginalVidComplRes` ("Video Complement to the Original").

**Resource fields for Live Photo video:**
- `resOriginalVidComplRes` — full-quality QuickTime video (the Live Photo motion)
- `resOriginalVidComplFileType` — `com.apple.quicktime-movie`
- `resOriginalVidComplFileSize` — size in bytes
- `resOriginalVidComplWidth/Height` — video dimensions
- `resVidMedRes`, `resVidSmallRes` — transcoded smaller variants

**Implications for sync:**
- Live Photo = 1 CPLMaster with 2 downloadable resources (still + video)
- To download both: fetch `resOriginalRes` for the HEIC, `resOriginalVidComplRes` for the MOV
- Presence of `resOriginalVidComplRes` is how you detect a Live Photo
- Standard (non-Live) photos have `resOriginalRes` only, no `VidCompl` fields

### Test 3: Pagination, Album Membership, Hidden Photo (7 API calls)

After test 2, user performed 3 types of changes. Detection used paginated variant with `resultsLimit=3` to force multi-page responses.

**Changes detected:** 6 new photos (IMG_0213–IMG_0218.HEIC), 1 photo added to existing album, 1 photo hidden. Total: 17 delta records across 6 pages, 7 API calls.

#### Pagination Behavior (resultsLimit=3)

Each page returned exactly `resultsLimit` records (except the final page with 2), and each page produced a **new unique syncToken**:

```
Page 1: 3 records (CPLAlbum + CPLAsset + CPLContainerRelation), moreComing=true, token_1
Page 2: 3 records (CPLAsset×2 + CPLMaster×1), moreComing=true, token_2
Page 3: 3 records (CPLAsset×1 + CPLMaster×2), moreComing=true, token_3
Page 4: 3 records (CPLAsset×2 + CPLMaster×1), moreComing=true, token_4
Page 5: 3 records (CPLAsset×1 + CPLMaster×2), moreComing=true, token_5
Page 6: 2 records (CPLAsset×1 + CPLLibraryInfo×1), moreComing=false, token_6
```

7 unique tokens total (initial + 6 pages). Record types are **interleaved** across pages, not grouped — a single page can contain CPLAlbum + CPLAsset + CPLContainerRelation. Each intermediate token is fully usable as a resume point.

#### Album Membership (Adding Photo to Existing Album)

Adding a photo to an **existing** album produces **2 records**:

1. **CPLAlbum** — The album record itself is modified (updated `recordModificationDate`):
   ```json
   {
       "recordType": "CPLAlbum",
       "recordName": "AAAAAAAA-BBBB-4CCC-DDDD-EEEEEEEEEEEE",
       "fields": {
           "albumNameEnc": {"value": "...base64 album name..."},
           "albumType": {"value": ...},
           "sortAscending": {"value": ...},
           "sortType": {"value": ...},
           "sortTypeExt": {"value": ...},
           "position": {"value": ...},
           "recordModificationDate": {"value": ...},
           "importedByBundleIdentifierEnc": {"value": ...},
           "userModificationDate": {"value": ...}
       }
   }
   ```

2. **CPLContainerRelation** — The membership link between photo and album:
   ```json
   {
       "recordType": "CPLContainerRelation",
       "recordName": "{assetUUID}-IN-{albumUUID}",
       "fields": {
           "containerId": {"value": ...},
           "itemId": {"value": ...},
           "isKeyAsset": {"value": ...},
           "position": {"value": ...},
           "recordModificationDate": {"value": ...}
       }
   }
   ```

**Key findings:**
- Adding to an existing album updates the CPLAlbum record (it's a modification, not a creation)
- The `recordName` pattern for CPLContainerRelation is `{assetUUID}-IN-{albumUUID}`
- `containerId` = album UUID, `itemId` = asset UUID
- `isKeyAsset` indicates if this is the album's cover photo
- No CPLMaster or CPLAsset modifications — album membership is entirely in CPLContainerRelation

**Implications for sync:**
- Album changes don't affect photo downloads — no re-download needed
- To track album membership, need to process CPLContainerRelation records
- Album name is in `albumNameEnc` (base64 encoded) on the CPLAlbum record

#### Hidden Photo (isHidden field)

Hiding a photo modifies only the **CPLAsset** — same pattern as `isDeleted`:

```json
{
    "recordType": "CPLAsset",
    "recordName": "11111111-2222-4333-4444-555555555555",
    "fields": {
        "isHidden": {"value": 1, "type": "INT64"},
        ...all other fields unchanged...
    }
}
```

**Key findings:**
- `isHidden: 1` on the CPLAsset is the signal — mirrors the `isDeleted` pattern
- CPLMaster is NOT modified (the binary file is unchanged)
- No separate record type — it's a field update on the existing CPLAsset
- The photo disappears from `CPLAssetAndMasterByAssetDateWithoutHiddenOrDeleted` queries (note "WithoutHidden" in the name)
- Hidden photos are queryable via `CPLAssetHiddenByAssetDate` record type and counted via `HyperionIndexCountLookup` with `indexCountID: "CPLAssetHiddenByAssetDate"`

**Implications for sync:**
- Detect hidden via: `fields.isHidden.value == 1` on CPLAsset
- Un-hiding sets `isHidden` back to `0` (not `null` — different from `isDeleted` which uses `null`). See Test 4.
- The current full-scan query (`WithoutHiddenOrDeleted`) already excludes hidden photos — so they're invisible to our current sync. syncToken delta will surface them.

#### CPLLibraryInfo (Library Metadata)

Every change to the library produces a **CPLLibraryInfo** update with global counters:

```json
{
    "recordType": "CPLLibraryInfo",
    "recordName": "PrimarySync-0000-LI",
    "fields": {
        "photosCount": {"value": ...},
        "videosCount": {"value": ...},
        "hiddenPhotosCount": {"value": ...},
        "hiddenVideosCount": {"value": ...},
        "albumsCount": {"value": ...},
        "memoriesCount": {"value": ...},
        "othersCount": {"value": ...},
        "audiosCount": {"value": ...},
        "irisCount": {"value": ...},
        "burstsExcludedFromPhotosCount": {"value": ...},
        "v2PhotosCount": {"value": ...},
        "v2VideosCount": {"value": ...},
        "featureVersion": {"value": ...},
        "libraryInfoVersion": {"value": ...},
        "lastSyncedToken": {"value": ...},
        "linkedShareZoneNameList": {"value": ...},
        "linkedShareZoneOwnerList": {"value": ...},
        ...
    }
}
```

**Key findings:**
- Always present in deltas when photos/albums change
- Contains library-wide counts — could be used for quick sanity checks
- `lastSyncedToken` field exists on this record (Apple's own sync tracking)
- `linkedShareZoneNameList`/`linkedShareZoneOwnerList` — shared library zone references

#### Multiple Zones Changed

The `changes/database` pre-check revealed **3 zones** with changes:
- `PrimarySync` — the main private library (what we sync)
- `SharedSync-{UUID}` — shared library zone
- `AppLibrarySync-com.apple.GenerativePlayground-{UUID}` — Apple Intelligence zone

We only query `PrimarySync` in `changes/zone`. The other zones are informational but show that iCloud Photos uses multiple zones internally.

### Test 4: Un-hide, Batch Delete, Album Removal, Favorite (14 API calls)

After test 3, user performed 4 operations. Detection used paginated variant (resultsLimit=3), producing 13 pages and 37 delta records.

**Changes detected:**
- 15 files batch-deleted (soft delete)
- 1 photo un-hidden
- 1 photo favorited
- 1 photo removed from album
- 1 CPLContainerRelation hard-deleted (album membership removal)
- 1 CPLAlbum updated
- 1 CPLLibraryInfo updated

#### Un-hide (isHidden 1 → 0)

Un-hiding a photo sets `isHidden` from `1` back to `0`:

```json
{
    "recordType": "CPLAsset",
    "recordName": "11111111-2222-4333-4444-555555555555",
    "fields": {
        "isHidden": {"value": 0, "type": "INT64"},
        ...
    }
}
```

**Key finding:** `isHidden` goes to `0`, NOT `null`. This is different from `isDeleted` which goes to `null` on restore. The CPLMaster is untouched.

#### Favorite (isFavorite field)

Favoriting a photo sets `isFavorite: 1` on the CPLAsset:

```json
{
    "recordType": "CPLAsset",
    "recordName": "66666666-7777-4888-9999-AAAAAAAAAAAA",
    "fields": {
        "isFavorite": {"value": 1, "type": "INT64"},
        ...
    }
}
```

**Key finding:** Same INT64 pattern as `isHidden`. CPLMaster is untouched. Un-favoriting would set `isFavorite` back to `0` (not `null`, based on the `isHidden` pattern).

#### Album Removal (CPLContainerRelation hard-deleted)

Removing a photo from an album produces a **hard delete** of the CPLContainerRelation:

```json
{
    "recordName": "22222222-3333-4444-5555-666666666666-IN-AAAAAAAA-BBBB-4CCC-DDDD-EEEEEEEEEEEE",
    "deleted": true
}
```

**Key findings:**
- The CPLContainerRelation is **hard-deleted** (`deleted: true`, `recordType: null`) — not soft-deleted
- The `recordName` pattern `{assetUUID}-IN-{albumUUID}` lets you parse out which photo was removed from which album
- The CPLAlbum record is also updated (modification timestamp changes)
- The photo's CPLAsset and CPLMaster are NOT modified — only the membership link is removed
- This is the first case where `deleted: true` appears on something other than a purged photo

**Implication:** When processing hard-deleted records, check if the `recordName` contains `-IN-` — if so, it's an album membership removal, not a photo purge.

#### Batch Delete (15 photos)

Deleting 15 photos at once produced 30 soft-deleted records (15 CPLMaster + 15 CPLAsset). Each photo is **individually represented** — no coalescing or batching by the API. The records were spread across multiple pages when paginated.

**Key finding:** Batch operations are not special. Each photo is its own pair of records regardless of whether the user selected them all at once.

### Advanced Token Tests (Round 1 — Automated, No User Action)

13 automated tests covering token edge cases. 10 pass, 1 fail (bug in test), 5 warn.

| # | Test | Result | Finding |
|---|------|--------|---------|
| 1 | Token stability (caught-up) | PASS | 0 records, moreComing=false |
| 2 | Token idempotent | PASS | Input token == output token when caught up |
| 3 | Cross-endpoint equivalence | PASS | `records/query` and `changes/zone` return **identical** caught-up tokens. Note: `records/query` requires `resultsLimit >= 2` |
| 4 | Concurrent clients | PASS | Two parallel requests with same token → identical results + identical tokens |
| 5 | changes/database bootstrap | PASS | No-token call returns all zones (PrimarySync, SharedSync-{UUID}, AppLibrarySync-{UUID}) + DB token |
| 6 | changes/database caught-up | PASS | Re-query with DB token → 0 zones changed |
| 6b | DB token stability | WARN | DB token changes even with 0 zone changes (non-deterministic) |
| 7 | Token namespace independence | PASS | DB and zone tokens are different formats/namespaces. DB token rejected by `changes/zone` with `BAD_REQUEST` |
| 8 | Multi-zone request | WARN | `SharedSync` (without UUID suffix) → `ZONE_NOT_FOUND`. Must use full zone name from `changes/database` |
| 9 | Empty string token | SPECIAL | Returns 0 records, moreComing=false — treated as caught-up, NOT as full history |
| 10 | Omitted token | PASS | Full history enumeration (0 records first page, moreComing=true) |
| 10b | Empty vs omitted | DIFFERENT | Empty string ≠ omitted. Empty = caught-up; omitted = full history |
| 11 | Garbage token (valid base64) | PASS | Rejected: `BAD_REQUEST — Unknown sync continuation type` |
| 12 | Truncated real token | PASS | Rejected: `BAD_REQUEST — Invalid continuation format` |
| 13 | Non-existent zone | PASS | `ZONE_NOT_FOUND` |

**Critical findings from automated tests:**
- **Empty string token is NOT the same as omitting the token.** Empty string = caught-up state (0 records, done). No token = full history enumeration. This matters for implementation: use `Option<String>` with `None` (not empty string) for "no token".
- **DB tokens and zone tokens are incompatible.** Different namespaces. Cannot mix them.
- **Shared zone names include UUIDs** — must discover via `changes/database`, not hardcode.
- **DB token is non-deterministic** — it may change even when nothing happened. Don't rely on DB token stability for caching.
- **Concurrent access is safe** — two clients using the same token simultaneously get identical results.

### Test 5: Un-favorite, Duplicate Photo, Personal Album Add (4 API calls)

Delta: 7 records across 3 pages.

#### Un-favorite (isFavorite 1 → 0)

`66666666` now has `isFavorite: 0`. Confirmed: un-favorite goes from `1` to `0`, matching the `isHidden` pattern (not `null` like `isDeleted`).

#### Duplicate Photo (iOS "Duplicate" option)

iOS duplicate creates a **new CPLAsset** (`ABCD1234`) sharing the **same CPLMaster** (`AExmplFingerprint1abcdefghi` = `IMG_0209.JPG`). No new CPLMaster is created — iCloud deduplicates at the master level.

```
CPLMaster AExmplFi... → IMG_0209.JPG (single binary, fingerprint matches recordName)
  ↑ masterRef
CPLAsset ABCD1234... (new — the duplicate)
CPLAsset CCCCCCCC... (existing — the original)
```

**Key findings:**
- **No new binary uploaded** — the CPLMaster is reused, not duplicated
- Only 1 CPLMaster + 1 new CPLAsset in the delta (not 2 CPLMasters)
- The duplicate CPLAsset has its own `recordName`, `addedDate`, etc.
- `resOriginalFingerprint` on the shared CPLMaster is identical — same file content
- For sync purposes: the duplicate appears as a new photo (new CPLAsset) but the download URL is the same master

### Test 6: Shared Album Add (2 API calls)

Adding a photo to a **shared album** behaves differently from personal albums.

Delta in **PrimarySync**: 1 record — only a **CPLAsset update** with sharing metadata:

```json
{
    "recordType": "CPLAsset",
    "recordName": "CCCCCCCC-DDDD-4EEE-FFFF-111111111111",
    "fields": {
        "lastSharedDate": {"value": 1773156957000, "type": "TIMESTAMP"},
        "sharedSyncSharingStateEnc": {"value": "eGFtcGxl...", "type": "ENCRYPTED_BYTES"},
        "shareCount": {"value": 0, "type": "INT64"},
        ...
    }
}
```

**Key findings:**
- **No CPLContainerRelation** in PrimarySync (unlike personal albums which create one)
- **No CPLAlbum** update in PrimarySync
- **No CPLMaster** change
- Only the CPLAsset is updated with `lastSharedDate` and `sharedSyncSharingStateEnc`
- The **SharedSync-{UUID}** zone is also flagged in `changes/database` — the actual shared album membership record lives there
- `shareCount: 0` — may increment when recipient accepts, or may count something else

**Implications for sync:**
- Shared album membership is split across zones: PrimarySync gets a metadata flag, SharedSync gets the actual membership
- For downloading shared photos from others: would need to query the SharedSync zone separately
- For tracking "which of my photos are shared": check `lastSharedDate` on CPLAsset in PrimarySync

---

## SharedSync Zone (iCloud Shared Photo Library)

### Discovery

The SharedSync zone lives in the **`/private` endpoint** (not `/shared`). It's discovered via `/zones/list`:

```json
{
    "zones": [
        {"zoneID": {"zoneName": "PrimarySync", "ownerRecordName": "_aabbccdd11223344556..."}},
        {"zoneID": {"zoneName": "SharedSync-12345678-ABCD-4EF0-9012-3456789ABCDE", "ownerRecordName": "_aabbccdd11223344556..."}}
    ]
}
```

The zone name includes a UUID suffix that is unique to the shared library. Must discover dynamically — cannot hardcode.

**Note:** The `/shared` endpoint (`/production/shared` instead of `/production/private`) returns **0 zones**. Traditional shared albums (invite-specific-people albums) may use a different mechanism entirely or may be embedded in PrimarySync. The SharedSync zone is specifically Apple's **iCloud Shared Photo Library** feature (family-wide shared library).

### Same API, Same Record Types

SharedSync supports all the same operations as PrimarySync:

| Operation | Works? | Notes |
|-----------|--------|-------|
| `records/query` | Yes | Same query format, same record types |
| `changes/zone` | Yes | Full history enumeration + incremental delta |
| `HyperionIndexCountLookup` | Yes | Returns photo count for SharedSync zone |
| syncToken | Yes | Returned by `records/query`, usable in `changes/zone` |

Record types are mostly identical: `CPLAsset`, `CPLMaster`, `CPLLibraryInfo` (and presumably `CPLAlbum`, `CPLContainerRelation`, etc.). SharedSync has one additional type: `CPLSharedLibraryQuota`.

### Key Differences from PrimarySync

1. **`contributors` field** — Present on both CPLAsset and CPLMaster. Contains the `ownerRecordName` of the user who added the photo. Not present in PrimarySync records.
   ```json
   "contributors": {
       "value": [{"recordName": "_aabbccdd1122334455667788aabbccdd", "action": "NONE"}]
   }
   ```

2. **`CPLSharedLibraryQuota` record type** — Tracks storage usage per contributor. Contains `contributedQuotaInSharedLibrary` field. Only in SharedSync.

3. **Scale** — SharedSync can be much larger than PrimarySync. In our test library: PrimarySync ~7,300 photos, SharedSync **11,698 photos**. The shared library includes contributions from all family members.

4. **Zone name includes UUID** — `SharedSync-{UUID}` vs just `PrimarySync`. Must use `/zones/list` to discover the full name.

5. **`deletedBy` field** — Present on CPLAsset when a photo is removed from shared library. Contains the `ownerRecordName` of the person who removed it. Only on CPLAsset, not CPLMaster.

6. **Missing fields** — SharedSync CPLAsset records lack some fields present in PrimarySync (e.g., `people`/`facesVersion` absent).

### Download from SharedSync (Verified)

SharedSync download URLs work identically to PrimarySync — same auth cookies, same endpoint format.

- **With cookies**: HTTP 200, full file downloaded (verified 11.9 MB DNG file)
- **Without cookies**: HTTP 200 but `Content-Length: 17` — returns a 17-byte error body (501 status in response body)
- Download URL format: same `cvws-h2.icloud-content.com` domain as PrimarySync
- No additional authentication needed beyond the standard session cookies

**Implication:** SharedSync photos can be downloaded with the existing download infrastructure. No changes to `download/mod.rs` download logic needed.

### Probed Data (Full History Enumeration)

First 5 pages (1,000 records) of SharedSync `changes/zone`:
- Page 1: 200 records (158 CPLAsset + 42 CPLMaster)
- Pages 2-5: 200 CPLAsset each
- Total at 5 pages: 958 CPLAsset + 42 CPLMaster
- `moreComing: true` — many more records exist

Photo count via `HyperionIndexCountLookup`: **11,698**

`records/query` with `CPLAssetAndMasterByAssetDateWithoutHiddenOrDeleted` returns results with syncToken, just like PrimarySync.

### SharedSync Incremental Delta (Verified)

Baseline → user adds 2 photos to shared library (1 existing + 1 new) → detect:
- `changes/database` correctly flags SharedSync zone
- `changes/zone` returns 6 records: 2 CPLMaster + 2 CPLAsset + 1 CPLLibraryInfo + 1 CPLSharedLibraryQuota
- Token mechanics identical to PrimarySync: 2 API calls total
- `contributors` field present on delta records, confirming who added the photos
- Adding an existing photo to SharedSync and taking a new photo directly into SharedSync produce **identical delta records** — indistinguishable in the API

### Multi-User Shared Library Behavior (Verified)

Tested with another family member making changes to the shared library.

#### Another person takes a photo into shared library

SharedSync delta: new CPLMaster + CPLAsset + CPLLibraryInfo + CPLSharedLibraryQuota (4 records).

```json
{
    "recordType": "CPLMaster",
    "recordName": "AExmplSharedRec1abcdefghijk",
    "fields": {
        "filenameEnc": {"value": "SU1HXzcxNzMuSEVJQw=="},
        "contributors": {
            "value": [{"recordName": "_11223344aabbccdd5566eeff77889900", "action": "NONE"}],
            "type": "REFERENCE_LIST"
        },
        "resOriginalFingerprint": {"value": "AExmplSharedRec1abcdefghijk"},
        ...
    }
}
```

**Key findings:**
- The photo appears in YOUR SharedSync delta — other people's contributions are visible
- `contributors` contains **their** user ID (not yours)
- PrimarySync delta: **0 records** — no cross-zone effect
- The photo exists ONLY in SharedSync, not in your PrimarySync

#### Another person removes a photo from shared library

SharedSync delta: soft delete (`isDeleted: 1`) on both CPLMaster and CPLAsset.

New field **`deletedBy`** on CPLAsset (SharedSync-only):
```json
{
    "recordType": "CPLAsset",
    "fields": {
        "isDeleted": {"value": 1},
        "deletedBy": {
            "value": {"recordName": "_11223344aabbccdd5566eeff77889900"},
            "type": "REFERENCE"
        },
        "contributors": {
            "value": [{"recordName": "_11223344aabbccdd5566eeff77889900"}],
            "type": "REFERENCE_LIST"
        }
    }
}
```

**Key findings:**
- Removal from shared = soft delete in SharedSync (same `isDeleted: 1` pattern)
- `deletedBy` field tracks WHO removed it — only on CPLAsset, not CPLMaster
- PrimarySync delta: **0 records** — complete zone isolation
- The photo moves to their personal library; it doesn't appear in yours at all

#### Another person DELETES a photo from shared library (vs moving to personal)

SharedSync delta: identical to "move to personal" — soft delete with `isDeleted: 1` and `deletedBy`. The **only difference** is `trashReason`:

| Field | Move to personal | Delete |
|-------|-----------------|--------|
| `isDeleted` | `1` | `1` |
| `deletedBy` | their user ID | their user ID |
| `trashReason` | absent/null | **`1`** |

#### trashReason Values (SharedSync only)

| `trashReason` | Meaning | PrimarySync Effect |
|---------------|---------|-------------------|
| `0` or absent | Moved to personal library (still exists somewhere) | New records if YOUR photo; 0 records if theirs |
| `1` | Actually deleted (goes to Recently Deleted → purge after 30 days) | 0 records |

- `deletedBy` always identifies WHO initiated the action
- `isExpunged: 1` may accompany moves (observed on your own move-to-personal)
- PrimarySync: 0 records when another person moves/deletes; new CPLMaster+CPLAsset when YOU move back to personal

### Cross-Zone Behavior (Critical for Implementation)

When **YOU** add a photo to the Shared Library, records appear in **BOTH** zones:

```
Action: You take new photo into Shared Library
  PrimarySync delta:  CPLMaster + CPLAsset + CPLLibraryInfo
  SharedSync delta:   CPLMaster + CPLAsset + CPLLibraryInfo + CPLSharedLibraryQuota

Action: You add existing photo to Shared Library
  PrimarySync delta:  CPLMaster (updated) + CPLAsset
  SharedSync delta:   CPLMaster (new) + CPLAsset + CPLLibraryInfo + CPLSharedLibraryQuota
```

When **YOU** move a photo from Shared back to Personal:

```
Action: You move your photo from Shared → Personal
  PrimarySync delta:  CPLMaster (new) + CPLAsset (new) + CPLLibraryInfo   ← photo "arrives"
  SharedSync delta:   CPLMaster (isDeleted=1) + CPLAsset (isDeleted=1, deletedBy=YOU, trashReason=0, isExpunged=1)
                      + CPLLibraryInfo + CPLSharedLibraryQuota
```

When **ANOTHER PERSON** adds/removes a photo:

```
Action: They take a photo into Shared Library
  Your PrimarySync delta:  0 records (no effect)
  Your SharedSync delta:   CPLMaster + CPLAsset (their contributor ID)

Action: They move a photo from Shared → Personal
  Your PrimarySync delta:  0 records (no effect)
  Your SharedSync delta:   CPLMaster (isDeleted=1) + CPLAsset (isDeleted=1, deletedBy=them, trashReason=0)

Action: They DELETE a photo from Shared Library
  Your PrimarySync delta:  0 records (no effect)
  Your SharedSync delta:   CPLMaster (isDeleted=1) + CPLAsset (isDeleted=1, deletedBy=them, trashReason=1)
```

**Deduplication key:** `CPLMaster.recordName == resOriginalFingerprint` across both zones. Verified: same photo in PrimarySync and SharedSync has identical CPLMaster `recordName` (e.g., `AExmplFingerprint1abcdefghi` = IMG_0209.JPG in both zones). This is the cross-zone dedup key.

**Deduplication rule:** Cross-zone duplication only happens for YOUR photos. Other people's photos exist ONLY in SharedSync. When syncing both zones, dedup your own photos by `resOriginalFingerprint` (which equals `recordName` on CPLMaster).

**Implications for sync:**
- `--library personal` / `--library shared` / `--library both` flags to let user choose
- If syncing both: your photos appear in both zones — dedup by fingerprint
- Other people's photos only appear in SharedSync — no dedup needed
- `contributors` field distinguishes your photos from others'

### Implementation Plan for Shared Library Support

Supporting SharedSync requires minimal new code:

1. **Zone discovery**: Call `/zones/list`, find any zone starting with `SharedSync-`
2. **Parallel sync**: Run the same sync logic against both `PrimarySync` and `SharedSync-{UUID}`
3. **Separate syncTokens**: Store a per-zone token in metadata table (`sync_token_PrimarySync`, `sync_token_SharedSync-{UUID}`)
4. **Download path**: Shared library photos need a distinct directory or prefix to avoid filename collisions with PrimarySync
5. **`contributors` field**: Optionally track/display who added each photo
6. **CLI flag**: `--include-shared-library` or similar to opt-in

The existing `records/query` pagination, `changes/zone` delta detection, and all syncToken mechanics work identically. No new API endpoints needed.

---

## Date Field Format

Dates in CloudKit responses use milliseconds since Unix epoch (January 1, 1970 00:00:00 UTC).

```json
{
    "addedDate": {"value": 1773111052938, "type": "TIMESTAMP"},
    "assetDate": {"value": 1773111052938, "type": "TIMESTAMP"}
}
```

Convert: `1773111052938 / 1000 = 1773111052.938` → `2026-03-10 02:50:52.938 UTC`

**Note:** `addedDate` is on CPLAsset records (not CPLMaster). `filenameEnc` is on CPLMaster records (not CPLAsset). They're linked via the `masterRef` field on CPLAsset.

Filename encoding: base64 of the actual filename. `SU1HXzAyMDcuRE5H` → `IMG_0207.DNG`

---

## Codebase Changes Required

### Minimal (capture token — zero new API calls)

1. **`cloudkit.rs`** — Add `sync_token` to `QueryResponse`:
   ```rust
   pub(crate) struct QueryResponse {
       #[serde(default)]
       pub records: Vec<Record>,
       #[serde(default, rename = "syncToken")]
       pub sync_token: Option<String>,
       #[serde(default, rename = "continuationMarker")]
       pub continuation_marker: Option<String>,
   }
   ```

2. **`album.rs`** — Propagate token from `QueryResponse` through photo stream

3. **`state/`** — Store token in metadata table after sync completes

### Full Incremental Sync

4. **New `changes/zone` request/response types** in `cloudkit.rs`:
   ```rust
   pub(crate) struct ChangesZoneRequest {
       pub zones: Vec<ChangesZoneEntry>,
   }
   pub(crate) struct ChangesZoneEntry {
       pub zone_id: Value,
       #[serde(skip_serializing_if = "Option::is_none")]
       pub sync_token: Option<String>,
       pub results_limit: u32,
   }
   pub(crate) struct ChangesZoneResponse {
       pub zones: Vec<ChangesZoneResult>,
   }
   pub(crate) struct ChangesZoneResult {
       pub zone_id: Value,
       pub sync_token: String,
       pub more_coming: bool,
       pub records: Vec<Record>,
   }
   ```

5. **New `changes/database` types**:
   ```rust
   pub(crate) struct ChangesDatabaseResponse {
       pub sync_token: String,
       pub more_coming: bool,
       pub zones: Vec<ChangedZoneInfo>,
   }
   pub(crate) struct ChangedZoneInfo {
       pub zone_id: Value,
       pub sync_token: String,
   }
   ```

6. **Sync logic** — Check for stored token, branch between full scan and incremental

7. **Deletion handling** — Match deleted recordNames against local state DB to remove/mark files

---

## Features Enabled by syncToken

1. **Incremental sync** — Only process new/changed/deleted photos (O(changes) vs O(library))
2. **Fast no-change detection** — Single API call to confirm nothing changed
3. **Server-side deletion** — `--auto-delete` flag: detect local files not in iCloud, delete them
4. **Two-way sync** — Detect iCloud deletions and propagate locally (or vice versa)
5. **Change notifications** — "3 new photos since last sync" style reporting
6. **Resumable sync** — Crash at any point, resume from last stored token
7. **Watch mode optimization** — No-change cycles drop from ~75 API calls to 1

