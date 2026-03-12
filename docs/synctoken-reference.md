# CloudKit syncToken Reference

This document describes how Apple's iCloud Photos service uses CloudKit's syncToken mechanism for change tracking. It covers the relevant API endpoints, the record types used by `com.apple.photos.cloud`, the deletion model, and the behavior of the SharedSync zone used for iCloud Shared Photo Library.

All tokens, record names, user IDs, and UUIDs are sanitized examples.

## syncToken

A syncToken is an opaque base64-like string, roughly 48 characters long, that represents a zone-level point-in-time position in the zone's mutation stream. Every record creation, modification, or deletion advances the stream. A consumer can provide a previously received syncToken to retrieve only the changes that have occurred since that position.

syncTokens are distinct from `continuationMarker`, which is a page cursor used within a single query. syncTokens are not tied to a session, user agent, or query offset - they can be persisted to disk and reused across sessions.

Example: `AaBbCcDdEeFf0123456789GgHhIiJjKkLlMmNn...`

## Endpoints

All iCloud Photos CloudKit endpoints share a common base URL and request format.

Base URL:

```
{ckdatabasews_url}/database/1/com.apple.photos.cloud/production/{library_type}
```

The `ckdatabasews_url` comes from the `accountLogin` response at `.webservices.ckdatabasews.url`. The `library_type` is `private` for the user's personal library or `shared` (though `shared` currently returns no useful data - see the SharedSync section for details).

All requests include these query parameters:

```
?clientId={url_encoded_client_id}&getCurrentSyncToken=true&remapEnums=true
```

The `getCurrentSyncToken=true` parameter is required to receive a syncToken in responses. Without it, no token is returned.

All requests use:

- Method: POST
- `Content-type: text/plain`
- `Origin: https://www.icloud.com`

### Endpoint Summary

| Endpoint | Purpose | syncToken role |
|----------|---------|----------------|
| `/records/query` | Query by index | Output only (in response) |
| `/changes/database` | Database-level zone change check | Input + output |
| `/changes/zone` | Zone-level record deltas | Input + output |
| `/records/changes` | Deprecated zone deltas | Input + output (same data as `/changes/zone`) |
| `/internal/records/query/batch` | Batch count queries (`HyperionIndexCountLookup`) | None |
| `/zones/list` | List available zones | None |

## Endpoints > /records/query

This is the endpoint existing iCloud Photos tools use for photo enumeration. It queries photos by index position (using `startRank`) and returns records along with a syncToken. The syncToken from this endpoint is interchangeable with the token used by `/changes/zone`, which means the first full scan already produces a usable change-tracking token at no extra cost.

### Request

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

The `recordType` value `CPLAssetAndMasterByAssetDateWithoutHiddenOrDeleted` is a server-side index that returns paired CPLAsset and CPLMaster records, excluding hidden and soft-deleted photos. Other index names are used for hidden photos (`CPLAssetHiddenByAssetDate`) and recently deleted photos (`CPLAssetAndMasterDeletedByExpungedDate`).

### Response

```json
{
    "records": [ ... ],
    "syncToken": "AaBbCcDdEeFf0123...",
    "continuationMarker": "AbCdEfGh..."
}
```

### Behavior

- `syncToken` is returned at the top level alongside `records`
- `getCurrentSyncToken=true` query param is required; without it, no token is returned
- The token is zone-level: same value regardless of `startRank` offset or `resultsLimit`
- `continuationMarker` is a page cursor, separate from `syncToken`

### Pagination

This endpoint uses `continuationMarker` (not syncToken) for paging through results:

```json
{
    "query": { ... },
    "continuationMarker": "marker_from_previous_response",
    "resultsLimit": 200
}
```

Both `syncToken` and `continuationMarker` may appear in the same response. They serve different purposes:

- `syncToken`: a zone-level change bookmark that stays the same across all pages of a query
- `continuationMarker`: a page cursor that advances with each page of results

## Endpoints > /changes/database

This endpoint answers one question: "which zones have changes since the last time I checked?" It's the cheapest call available and is useful as a pre-check before doing more expensive zone-level queries. If no zones are returned, nothing has changed and no further API calls are needed.

### Request (no prior token)

```json
{}
```

When called without a token, it returns all zones along with a database-level syncToken that can be used on subsequent calls.

### Response (zones with changes)

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

### Request (with prior token)

```json
{
    "syncToken": "db_level_token..."
}
```

### Response (no changes)

When nothing has changed since the provided token, the `zones` array is empty:

```json
{
    "syncToken": "db_level_token...",
    "moreComing": false,
    "zones": []
}
```

### Behavior

- Returns 0 zones when nothing has changed
- Has its own database-level syncToken, which is separate from zone-level tokens
- Each zone in the response includes its own syncToken
- DB tokens and zone tokens are incompatible namespaces - passing a DB token to `/changes/zone` returns `BAD_REQUEST`
- The DB token is non-deterministic: it may change between calls even when no zone changes have occurred

## Endpoints > /changes/zone

This is the main endpoint for incremental sync. It returns all record changes in a zone since a given syncToken, including creations, modifications, and deletions. Each call returns up to 200 records along with a new syncToken for the next page. Pagination continues until `moreComing` is `false`.

### Request (no prior token - full history)

Omitting the syncToken triggers a full enumeration of the zone's change history, starting from the beginning:

```json
{
    "zones": [{
        "zoneID": {"zoneName": "PrimarySync"},
        "resultsLimit": 200
    }]
}
```

### Request (with prior token)

```json
{
    "zones": [{
        "zoneID": {"zoneName": "PrimarySync"},
        "syncToken": "AaBbCcDdEeFf0123...",
        "resultsLimit": 200
    }]
}
```

### Response

The response contains the records that changed since the provided token, along with a new syncToken to use for the next page or for future calls:

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

The second record in this example is a hard-deleted (purged) record - `recordType` is `null` and `deleted` is `true`, meaning the original record data is gone. See the Deleted Records section for the full deletion model.

### Behavior

- `moreComing: true` means more pages are available - call again with the new `syncToken`
- Page until `moreComing: false` to reach the caught-up state
- At the caught-up state, repeated calls return 0 records with `moreComing: false`
- The caught-up token is stable: calling it again returns the same empty result
- Maximum effective page size is 200 records regardless of what `resultsLimit` is set to
- The `desiredRecordTypes` filter is unreliable on this endpoint - it may return record types that weren't requested. Filter client-side instead.
- The `desiredKeys` filter works correctly
- `recordChangeTag` is present on all records, including deleted ones

### Pagination

Token-chained pagination: each response's `syncToken` becomes the next request's `syncToken`:

```
CALL 1:  no token           -> 200 records, moreComing=true,  token_1
CALL 2:  token_1            -> 200 records, moreComing=true,  token_2
CALL 3:  token_2            -> 200 records, moreComing=true,  token_3
...
CALL N:  token_{N-1}        -> K records,   moreComing=false, token_N (caught-up)
CALL N+1: token_N           -> 0 records,   moreComing=false, token_N (stable)
```

The API sometimes returns pages with 0 records but `moreComing: true`. Long stretches of empty pages can occur (e.g., pages 27-96 all returning 0 records with `moreComing=true`) before records appear again. This appears to happen when the API walks through compacted internal log segments. Implementations must not treat empty pages as an end-of-data signal - always check `moreComing`.

Each page returns a new, unique syncToken. Any intermediate token can be stored and reused as a resume point if the process is interrupted. Record types are interleaved across pages, not grouped by type.

### Pagination data (resultsLimit=3, 17-record delta)

This example shows what pagination looks like with a small page size. Note how record types are mixed across pages:

```
Page 1: 3 records (CPLAlbum + CPLAsset + CPLContainerRelation), moreComing=true, token_1
Page 2: 3 records (CPLAsset x2 + CPLMaster x1), moreComing=true, token_2
Page 3: 3 records (CPLAsset x1 + CPLMaster x2), moreComing=true, token_3
Page 4: 3 records (CPLAsset x2 + CPLMaster x1), moreComing=true, token_4
Page 5: 3 records (CPLAsset x1 + CPLMaster x2), moreComing=true, token_5
Page 6: 2 records (CPLAsset x1 + CPLLibraryInfo x1), moreComing=false, token_6
```

7 unique tokens total (initial + 6 pages).

### Full enumeration data (~7,300 photo library)

A full history enumeration of a ~7,300 photo library produces a large number of records, including a substantial proportion of historical deletions:

| Metric | Value |
|--------|-------|
| Total change records | 42,787 |
| Total pages (resultsLimit=200, many empty) | ~450 |
| Effective records per non-empty page | ~200 |
| Hard-deleted records | 15,618 (36%) |
| Records with fields | ~27,169 |
| Records without fields (deleted/minimal) | ~15,618 |

## Token Properties

syncTokens have several properties that make them well-suited for persistent change tracking. These properties apply to both `/changes/zone` tokens and the interchangeable tokens from `/records/query`.

| Property | Description |
|----------|-------------|
| Zone-level invariant | Represents entire zone state, not a page position. Same token regardless of `startRank` offset. |
| Deterministic | Same token, same call -> same results |
| Idempotent | Using a token does not consume or invalidate it |
| Random access | Tokens support jumping to any position, forward or backward. No sequential requirement. |
| Session-independent | Tokens survive authentication refresh. Can be stored persistently. |
| Cross-endpoint compatible | syncToken from `records/query` is interchangeable with `changes/zone` tokens |
| Page-size independent | Changing `resultsLimit` does not affect record ordering or token meaning |
| Crash-safe | Store last token received, restart from there. Follows from determinism and idempotency. |

The combination of determinism, idempotency, and session-independence means tokens can be safely persisted to a database and reused across process restarts and re-authentication cycles.

## Token Edge Cases

Several edge cases in token handling require careful implementation:

| Condition | Behavior |
|-----------|----------|
| Empty string (`""`) | Returns 0 records, `moreComing=false`; treated as caught-up, NOT as full history |
| Omitted (field absent) | Full history enumeration (0 records first page, `moreComing=true`) |
| Garbage (valid base64, invalid token) | `BAD_REQUEST` - `Unknown sync continuation type` |
| Truncated real token | `BAD_REQUEST` - `Invalid continuation format` |
| DB token passed to `changes/zone` | `BAD_REQUEST` - different namespaces, incompatible |
| Non-existent zone | `ZONE_NOT_FOUND` |
| `SharedSync` (without UUID suffix) | `ZONE_NOT_FOUND`; must use full zone name from `changes/database` |
| Concurrent clients (same token, parallel requests) | Identical results and identical tokens returned |

The distinction between an empty string token and an omitted token is important: an empty string is treated as "already caught up" and returns nothing, while omitting the field entirely starts a full history enumeration. Implementations should represent "no token" as a null/absent value, not as an empty string.

Cross-endpoint token equivalence (`records/query` token used with `changes/zone`) requires `resultsLimit >= 2` on the `records/query` call.

### Full Token Test Matrix

13 automated tests covering token edge cases and boundary conditions:

| # | Test | Result | Finding |
|---|------|--------|---------|
| 1 | Token stability (caught-up) | PASS | 0 records, `moreComing=false` |
| 2 | Token idempotent | PASS | Input token == output token when caught up |
| 3 | Cross-endpoint equivalence | PASS | `records/query` and `changes/zone` return identical caught-up tokens. Note: `records/query` requires `resultsLimit >= 2` |
| 4 | Concurrent clients | PASS | Two parallel requests with same token -> identical results + identical tokens |
| 5 | `changes/database` bootstrap | PASS | No-token call returns all zones (`PrimarySync`, `SharedSync-{UUID}`, `AppLibrarySync-{UUID}`) + DB token |
| 6 | `changes/database` caught-up | PASS | Re-query with DB token -> 0 zones changed |
| 6b | DB token stability | WARN | DB token changes even with 0 zone changes (non-deterministic) |
| 7 | Token namespace independence | PASS | DB and zone tokens are different formats/namespaces. DB token rejected by `changes/zone` with `BAD_REQUEST` |
| 8 | Multi-zone request | WARN | `SharedSync` (without UUID suffix) -> `ZONE_NOT_FOUND`. Must use full zone name from `changes/database` |
| 9 | Empty string token | SPECIAL | Returns 0 records, `moreComing=false` - treated as caught-up, NOT as full history |
| 10 | Omitted token | PASS | Full history enumeration (0 records first page, `moreComing=true`) |
| 10b | Empty vs omitted | DIFFERENT | Empty string != omitted. Empty = caught-up; omitted = full history |
| 11 | Garbage token (valid base64) | PASS | Rejected: `BAD_REQUEST` - `Unknown sync continuation type` |
| 12 | Truncated real token | PASS | Rejected: `BAD_REQUEST` - `Invalid continuation format` |
| 13 | Non-existent zone | PASS | `ZONE_NOT_FOUND` |

## Record Types

iCloud Photos stores each photo as a pair of linked records: a CPLAsset (metadata like dates, flags, and album membership) and a CPLMaster (the file reference with download URLs, checksums, and dimensions). They're linked by the `masterRef` field on CPLAsset, which points to the corresponding CPLMaster's `recordName`.

The full set of record types seen in `changes/zone` enumeration of a ~7,300 photo library (42,787 records):

| Record Type | Description |
|-------------|-------------|
| `CPLAsset` | Photo/video metadata (dates, flags, item type, album membership, `masterRef` to its CPLMaster) |
| `CPLMaster` | Binary file reference (download URL via `resOriginalRes`, checksum, dimensions, filename via `filenameEnc`) |
| `CPLAlbum` | Album metadata (appears on both create and modify) |
| `CPLContainerRelation` | Album-to-asset membership link (1 record per photo-in-album relationship) |
| `CPLFaceCrop` | Face detection crop data |
| `CPLLibraryInfo` | Library-level metadata (updated on every change; contains library-wide photo/video/album counts) |
| `CPLMemory` | Memories feature data |
| `CPLPerson` | People identification data |
| `CPLSuggestion` | Sharing suggestions |
| `CPLSharedLibraryQuota` | Storage usage per contributor (SharedSync only) |
| `null` | Hard-deleted (purged) record; `record.deleted: true`. Soft deletes retain their original recordType. |

For photo sync, only CPLAsset and CPLMaster records need to be processed. The other types can be filtered out client-side (since `desiredRecordTypes` is unreliable on `/changes/zone`).

### Query Index Types

The `records/query` endpoint uses server-side index names as the `recordType` value. These indexes define which records are returned and how they're filtered and sorted:

| Index Name | Returns | Notes |
|------------|---------|-------|
| `CheckIndexingState` | Indexing status | Must return successfully before photo queries will work. Called once during library initialization. |
| `CPLAssetAndMasterByAssetDateWithoutHiddenOrDeleted` | CPLAsset + CPLMaster pairs | Primary photo enumeration. Excludes hidden and soft-deleted photos. "List" type - returns paired records. |
| `CPLAssetByAssetDateWithoutHiddenOrDeleted` | CPLAsset only | "Object" type variant of the above |
| `CPLAssetAndMasterDeletedByExpungedDate` | CPLAsset + CPLMaster pairs | Recently Deleted album |
| `CPLAssetDeletedByExpungedDate` | CPLAsset only | Recently Deleted album (object type) |
| `CPLContainerRelationLiveByAssetDate` | CPLContainerRelation | Album members sorted by asset date |
| `CPLAlbumByPositionLive` | CPLAlbum | Album listing sorted by position |

Smart folder indexes return photos matching specific criteria:

| Index Name | Folder |
|------------|--------|
| `CPLAssetInSmartAlbumByAssetDate:Favorite` | Favorites |
| `CPLAssetInSmartAlbumByAssetDate:Video` | Videos |
| `CPLAssetInSmartAlbumByAssetDate:Live` | Live Photos |
| `CPLAssetInSmartAlbumByAssetDate:Screenshot` | Screenshots |
| `CPLAssetInSmartAlbumByAssetDate:Panorama` | Panoramas |
| `CPLAssetInSmartAlbumByAssetDate:Slomo` | Slo-mo |
| `CPLAssetInSmartAlbumByAssetDate:Timelapse` | Time-lapse |
| `CPLAssetBurstStackAssetByAssetDate` | Bursts |
| `CPLAssetHiddenByAssetDate` | Hidden |

Each of the above has an `AndMaster` variant (e.g., `CPLAssetAndMasterInSmartAlbumByAssetDate:Timelapse`, `CPLBurstStackAssetAndMasterByAssetDate`, `CPLAssetAndMasterHiddenByAssetDate`) that returns paired CPLAsset + CPLMaster records instead of CPLAsset alone.

Photo counts for any index are available via `HyperionIndexCountLookup` on the `/internal/records/query/batch` endpoint. The `indexCountID` matches the index name (e.g., `CPLAssetDeletedByExpungedDate`, `CPLAssetHiddenByAssetDate`).

### Zone ID Structure

Zone IDs in requests and responses are JSON objects with the following fields:

```json
{
    "zoneName": "PrimarySync",
    "ownerRecordName": "_aabbccdd11223344556...",
    "zoneType": "REGULAR_CUSTOM_ZONE"
}
```

- `zoneName`: the zone name. `PrimarySync` for the personal library, `SharedSync-{UUID}` for shared library.
- `ownerRecordName`: the account owner identifier. Can be `_defaultOwner` or the full owner record name.
- `zoneType`: optional. `REGULAR_CUSTOM_ZONE` when present.

For requests, only `zoneName` is required. The other fields are optional and can be omitted.

## CPLMaster Fields

A CPLMaster record represents the binary file associated with a photo or video. Each CPLMaster is linked to one or more CPLAsset records (multiple CPLAssets can share one CPLMaster when a photo is duplicated in iOS).

### Identification

| Field | Type | Description |
|-------|------|-------------|
| `recordName` | string | The record's unique identifier. Equals `resOriginalFingerprint` for the same record. |
| `recordType` | string | `"CPLMaster"` |
| `recordChangeTag` | string | Opaque tag that changes on every modification |

### Core Fields

| Field | Type | Description |
|-------|------|-------------|
| `filenameEnc` | STRING or ENCRYPTED_BYTES | Base64-encoded original filename. Decode to get the filename (e.g., `SU1HXzAyMDcuRE5H` -> `IMG_0207.DNG`). |
| `itemType` | STRING | UTI string identifying the file type (e.g., `public.heic`, `public.jpeg`, `com.apple.quicktime-movie`) |
| `dataClassType` | INT64 | Data classification type |
| `originalOrientation` | INT64 | EXIF orientation value of the original file |

### Resource Fields

Resource fields follow a naming convention: `res{SizeVariant}{Property}`, where each size variant has five properties:

| Property suffix | Type | Description |
|----------------|------|-------------|
| `Res` | object | Contains `size` (bytes), `downloadURL`, `fileChecksum`, and optionally `wrappingKey` |
| `FileType` | STRING | UTI string for this variant's format |
| `Fingerprint` | STRING | Content fingerprint |
| `Width` | INT64 | Width in pixels |
| `Height` | INT64 | Height in pixels |

The `Res` property is an object containing the download information:

```json
{
    "resOriginalRes": {
        "value": {
            "size": 12345678,
            "downloadURL": "https://cvws-h2.icloud-content.com/...",
            "fileChecksum": "AExmplChecksum1abcdefghijkl",
            "wrappingKey": "AAAAAAAAAAAAAAAAAAAAAA=="
        }
    }
}
```

### Size Variants

There are 11 resource size variants on CPLMaster records. Not all variants are populated on every record - the available variants depend on the file type (photo vs video, Live Photo vs standard).

| Prefix | Description | Present on |
|--------|-------------|------------|
| `resOriginal` | Original full-resolution file | All photos and videos |
| `resOriginalAlt` | Alternative original (e.g., RAW+JPEG pair) | Photos with alternative originals |
| `resOriginalVidCompl` | "Video Complement" - the motion video of a Live Photo | Live Photos only |
| `resJPEGFull` | Full-size JPEG rendition (also the edited/adjusted version) | Photos |
| `resJPEGLarge` | Large JPEG rendition | Photos |
| `resJPEGMed` | Medium JPEG rendition | Photos |
| `resJPEGThumb` | Thumbnail JPEG rendition | Photos |
| `resVidFull` | Full-size video rendition | Videos |
| `resVidMed` | Medium video rendition | Videos and Live Photos |
| `resVidSmall` | Small video rendition (thumbnail) | Videos and Live Photos |
| `resSidecar` | Sidecar file (e.g., AAE adjustment data) | Photos with adjustments |

Photos and videos use different subsets of these variants:

| Asset type | Resource variants |
|------------|------------------|
| Standard photo | `resOriginal`, `resOriginalAlt`, `resJPEGFull`, `resJPEGLarge`, `resJPEGMed`, `resJPEGThumb`, `resSidecar` |
| Live Photo | All photo variants + `resOriginalVidCompl`, `resVidMed`, `resVidSmall` |
| Video | `resOriginal`, `resVidFull`, `resVidMed`, `resVidSmall` |

### Version Size Mapping

For download purposes, logical version names map to resource field prefixes:

**Photo versions:**

| Logical version | Field prefix | Description |
|----------------|--------------|-------------|
| Original | `resOriginal` | Original full-resolution file |
| Alternative | `resOriginalAlt` | Alternative original (RAW+JPEG) |
| Adjusted | `resJPEGFull` | Apple's pre-rendered edited version |
| Medium | `resJPEGMed` | Medium JPEG |
| Thumb | `resJPEGThumb` | Thumbnail JPEG |
| LiveOriginal | `resOriginalVidCompl` | Live Photo video (original quality) |
| LiveMedium | `resVidMed` | Live Photo video (medium quality) |
| LiveThumb | `resVidSmall` | Live Photo video (small/thumbnail) |

**Video versions:**

| Logical version | Field prefix | Description |
|----------------|--------------|-------------|
| Original | `resOriginal` | Original video file |
| Medium | `resVidMed` | Medium quality transcode |
| Thumb | `resVidSmall` | Small/thumbnail transcode |

### Recognized Item Types

The `itemType` field on CPLMaster contains a UTI string. Known values:

**Images:**
- `public.heic`, `public.heif` - HEIC/HEIF
- `public.jpeg` - JPEG
- `public.png` - PNG
- `org.webmproject.webp` - WebP
- `com.adobe.raw-image` - Adobe DNG
- `com.canon.cr2-raw-image`, `com.canon.crw-raw-image`, `com.canon.cr3-raw-image` - Canon RAW
- `com.sony.arw-raw-image` - Sony RAW
- `com.fuji.raw-image` - Fujifilm RAW
- `com.panasonic.rw2-raw-image` - Panasonic RAW
- `com.nikon.nrw-raw-image`, `com.nikon.raw-image` - Nikon RAW
- `com.pentax.raw-image` - Pentax RAW
- `com.olympus.raw-image`, `com.olympus.or-raw-image` - Olympus RAW

**Videos:**
- `com.apple.quicktime-movie` - QuickTime MOV

## CPLAsset Fields

A CPLAsset record represents the metadata for a photo or video. It links to its CPLMaster (the file) via the `masterRef` field. Multiple CPLAssets can reference the same CPLMaster (e.g., when a photo is duplicated in iOS).

### Identification

| Field | Type | Description |
|-------|------|-------------|
| `recordName` | string | UUID format (e.g., `AAAAAAAA-BBBB-4CCC-DDDD-EEEEEEEEEEEE`) |
| `recordType` | string | `"CPLAsset"` |
| `recordChangeTag` | string | Opaque tag that changes on every modification |

### Linkage

| Field | Type | Description |
|-------|------|-------------|
| `masterRef` | REFERENCE | Points to the CPLMaster record: `{"value": {"recordName": "AExmpl..."}}` |

### Date Fields

| Field | Type | Description |
|-------|------|-------------|
| `assetDate` | TIMESTAMP | When the photo/video was taken (milliseconds since Unix epoch) |
| `addedDate` | TIMESTAMP | When the photo was added to the iCloud library (milliseconds since Unix epoch) |

### Status Flags

| Field | Type | Active | Inactive | Notes |
|-------|------|--------|----------|-------|
| `isDeleted` | INT64 | `1` | `null` (absent) | Soft delete (moved to trash). Restored -> `null`. |
| `isHidden` | INT64 | `1` | `0` | Hidden from main library view. Un-hidden -> `0`. |
| `isFavorite` | INT64 | `1` | `0` | Marked as favorite. Un-favorited -> `0`. |
| `isExpunged` | INT64 | `1` | `0` or `null` | Permanent purge flag |
| `dateExpunged` | TIMESTAMP | timestamp | `null` | When the purge will occur / occurred |

### Deletion Fields

| Field | Type | Description |
|-------|------|-------------|
| `trashReason` | INT64 | Why the photo was deleted. SharedSync: `0`/absent = moved to personal, `1` = actually deleted. |
| `deletedBy` | REFERENCE | SharedSync only. Who removed the photo from the shared library. |

### Adjustment Fields (Photo Edits)

These fields are populated when a photo has been edited in the Photos app or another app. The original file (CPLMaster) is not modified; edits are non-destructive.

| Field | Type | Description |
|-------|------|-------------|
| `adjustmentType` | STRING | Edit type identifier (e.g., `com.apple.photo`). Non-null signals an edited photo. |
| `adjustmentRenderType` | INT64 | Render type code (e.g., `19968`) |
| `adjustmentCreatorCode` | STRING | App that made the edit (e.g., `com.apple.mobileslideshow` = Photos app) |
| `adjustmentCompoundVersion` | STRING | Version of the editing engine (e.g., `1.9.1`) |
| `adjustmentSimpleDataEnc` | ENCRYPTED_BYTES | Base64-encoded edit parameters |
| `adjustedMediaMetaDataEnc` | ENCRYPTED_BYTES | Base64-encoded EXIF for the adjusted rendition |
| `adjustmentTimestampEnc` | ENCRYPTED_BYTES | Base64-encoded edit timestamp |
| `adjustmentSourceType` | INT64 | Source type for the adjustment |
| `otherAdjustmentsFingerprint` | STRING | Fingerprint of additional adjustments |
| `customRenderedValue` | INT64 | `10` = image has been processed/edited |

### Location Fields

| Field | Type | Description |
|-------|------|-------------|
| `locationEnc` | ENCRYPTED_BYTES | Encrypted location data |
| `locationV2Enc` | ENCRYPTED_BYTES | Encrypted location data (v2 format) |
| `locationLatitude` | DOUBLE | Latitude (may be present alongside encrypted fields) |
| `locationLongitude` | DOUBLE | Longitude (may be present alongside encrypted fields) |

### Media Metadata

| Field | Type | Description |
|-------|------|-------------|
| `orientation` | INT64 | Current EXIF orientation |
| `duration` | DOUBLE | Duration in seconds (videos) |
| `timeZoneOffset` | INT64 | Timezone offset from UTC |
| `assetSubtype` | INT64 | Asset subtype classification |
| `assetSubtypeV2` | INT64 | Asset subtype v2 classification |
| `assetHDRType` | INT64 | HDR type indicator |
| `captionEnc` | ENCRYPTED_BYTES | Encrypted caption/description |
| `extendedDescEnc` | ENCRYPTED_BYTES | Encrypted extended description |
| `keywordsEnc` | ENCRYPTED_BYTES | Encrypted keywords |

### Burst Photo Fields

| Field | Type | Description |
|-------|------|-------------|
| `burstFlags` | INT64 | Burst mode flags |
| `burstFlagsExt` | INT64 | Extended burst flags |
| `burstId` | STRING | Groups photos in the same burst sequence |

### Video Complement Fields

These fields are present on CPLAsset records for videos that have complementary renditions:

| Field | Type | Description |
|-------|------|-------------|
| `vidComplDurValue` | INT64 | Duration value (numerator) |
| `vidComplDurScale` | INT64 | Duration scale (denominator) |
| `vidComplDispValue` | INT64 | Display duration value |
| `vidComplDispScale` | INT64 | Display duration scale |
| `vidComplVisibilityState` | INT64 | Visibility state of the video complement |

### Sharing Fields

| Field | Type | Description |
|-------|------|-------------|
| `lastSharedDate` | TIMESTAMP | When the photo was last shared |
| `sharedSyncSharingStateEnc` | ENCRYPTED_BYTES | Encrypted sharing state |
| `shareCount` | INT64 | Sharing counter |

### SharedSync-Only Fields

| Field | Type | Description |
|-------|------|-------------|
| `contributors` | REFERENCE_LIST | List of user IDs who contributed this photo. Present on both CPLAsset and CPLMaster in SharedSync. |
| `deletedBy` | REFERENCE | User ID of who removed this photo from the shared library. CPLAsset only. |

### Album Membership Fields (on CPLContainerRelation)

These fields are on CPLContainerRelation records, not CPLAsset, but are included here for completeness:

| Field | Type | Description |
|-------|------|-------------|
| `containerId` | STRING | Album UUID |
| `itemId` | STRING | Asset UUID |
| `isKeyAsset` | INT64 | Whether this photo is the album's cover |
| `position` | INT64 | Sort position within the album |

## DESIRED_KEYS

When making `records/query` requests, the `desiredKeys` array specifies which fields to include in the response. The complete list of field names requested by icloudpd-rs:

```
resJPEGFullWidth, resJPEGFullHeight, resJPEGFullFileType, resJPEGFullFingerprint, resJPEGFullRes,
resJPEGLargeWidth, resJPEGLargeHeight, resJPEGLargeFileType, resJPEGLargeFingerprint, resJPEGLargeRes,
resJPEGMedWidth, resJPEGMedHeight, resJPEGMedFileType, resJPEGMedFingerprint, resJPEGMedRes,
resJPEGThumbWidth, resJPEGThumbHeight, resJPEGThumbFileType, resJPEGThumbFingerprint, resJPEGThumbRes,
resVidFullWidth, resVidFullHeight, resVidFullFileType, resVidFullFingerprint, resVidFullRes,
resVidMedWidth, resVidMedHeight, resVidMedFileType, resVidMedFingerprint, resVidMedRes,
resVidSmallWidth, resVidSmallHeight, resVidSmallFileType, resVidSmallFingerprint, resVidSmallRes,
resSidecarWidth, resSidecarHeight, resSidecarFileType, resSidecarFingerprint, resSidecarRes,
itemType, dataClassType, filenameEnc, originalOrientation,
resOriginalWidth, resOriginalHeight, resOriginalFileType, resOriginalFingerprint, resOriginalRes,
resOriginalAltWidth, resOriginalAltHeight, resOriginalAltFileType, resOriginalAltFingerprint, resOriginalAltRes,
resOriginalVidComplWidth, resOriginalVidComplHeight, resOriginalVidComplFileType,
resOriginalVidComplFingerprint, resOriginalVidComplRes,
isDeleted, isExpunged, dateExpunged, remappedRef,
recordName, recordType, recordChangeTag,
masterRef, adjustmentRenderType,
assetDate, addedDate, isFavorite, isHidden,
orientation, duration,
assetSubtype, assetSubtypeV2, assetHDRType,
burstFlags, burstFlagsExt, burstId,
captionEnc, locationEnc, locationV2Enc, locationLatitude, locationLongitude,
adjustmentType, timeZoneOffset,
vidComplDurValue, vidComplDurScale, vidComplDispValue, vidComplDispScale,
keywordsEnc, extendedDescEnc, adjustedMediaMetaDataEnc, adjustmentSimpleDataEnc,
vidComplVisibilityState, customRenderedValue,
containerId, itemId, position, isKeyAsset
```

113 fields total. This list covers both CPLAsset and CPLMaster fields - the same `desiredKeys` array is sent for all queries regardless of record type. Fields that don't apply to a given record type are returned as null or absent.

## Deleted Records

Deletion in the iCloud Photos API has two distinct levels, and they produce very different data in the API response. Understanding both is necessary to correctly track photo lifecycle.

### Soft Delete (Moved to Trash)

When a user deletes a photo, it moves to "Recently Deleted" (a 30-day trash). The record is modified, not removed. Both the CPLAsset and CPLMaster appear in the `changes/zone` delta as normal records with `deleted: false` at the record level and `fields.isDeleted.value == 1` inside the record's fields.

This is the common case for user-initiated deletions. A single photo deletion generates 2 delta records (1 CPLMaster + 1 CPLAsset, both with `isDeleted: 1`).

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
        "...all other fields still present..."
    },
    "recordChangeTag": "tag_abc"
}
```

Properties of a soft-deleted record:

- `record.deleted` is `false` - the record is still in the zone
- `record.recordType` retains its original type (`CPLMaster` or `CPLAsset`)
- `record.fields` is fully populated - all metadata, download URLs, etc. are still accessible
- `fields.isDeleted.value == 1` is the signal that this photo has been trashed
- `fields.isExpunged.value == 0` - not yet permanently removed
- `fields.dateExpunged` is null until the 30-day window expires
- `fields.trashReason` is present on CPLAsset records (indicates why the photo was deleted)
- `filenameEnc` is on CPLMaster; CPLAsset has `filenameEnc: null` (this is always the case, not specific to deletion)

### Hard Delete (Purged / Expunged)

After the 30-day trash window (or manual "Delete All" from Recently Deleted), records are permanently removed from the zone. These appear in the `changes/zone` delta with `deleted: true` at the record level, and all record data is gone:

```json
{
    "recordName": "GHI789",
    "recordType": null,
    "deleted": true,
    "recordChangeTag": "tag_def"
}
```

Properties of a hard-deleted record:

- `record.deleted` is `true`
- `record.recordType` is `null` - the original type is not recoverable
- `record.fields` is absent or null - no metadata is available
- `recordChangeTag` is still present
- These records appear in full history enumeration or after the 30-day expiry
- A ~7,300 photo library contained 15,618 hard-deleted records out of 42,787 total (36%)

### Hard Delete of CPLContainerRelation

Removing a photo from an album is handled differently from deleting a photo. Instead of a soft delete, the album membership record (CPLContainerRelation) is hard-deleted:

```json
{
    "recordName": "22222222-3333-4444-5555-666666666666-IN-AAAAAAAA-BBBB-4CCC-DDDD-EEEEEEEEEEEE",
    "deleted": true
}
```

The `recordName` follows the pattern `{assetUUID}-IN-{albumUUID}`, which makes it possible to determine which photo was removed from which album even though `recordType` is `null`.

### Detection Logic

To classify a record from a `changes/zone` delta:

```
if record.deleted == true:
    -> HARD DELETE (purged, recordType unknown)
    Also used for CPLContainerRelation removal (album membership)
elif record.fields.isDeleted.value == 1:
    -> SOFT DELETE (trashed, full record available)
elif record.fields.isHidden.value == 1:
    -> HIDDEN (moved to Hidden album, full record available)
elif record.fields.adjustmentType != null:
    -> EDITED (non-destructive edit, CPLAsset only, CPLMaster unchanged)
elif record.fields.isFavorite.value == 1:
    -> FAVORITED (CPLAsset only)
else:
    -> NEW or MODIFIED
```

### Field Value Asymmetry

The boolean-like fields on CPLAsset records use inconsistent representations for their "off" state:

| Field | Active value | Inactive value | Notes |
|-------|-------------|---------------|-------|
| `isDeleted` | `1` | `null` (field absent) | Restored photos: `1` -> `null` |
| `isHidden` | `1` | `0` | Un-hidden photos: `1` -> `0` |
| `isFavorite` | `1` | `0` | Un-favorited: `1` -> `0` |
| `isExpunged` | `1` | `0` or `null` | Permanent purge flag |

`isDeleted` is the outlier: it uses `null` (field absent) to mean "not deleted," while `isHidden` and `isFavorite` use an explicit `0`. Code that treats all these fields the same way will mishandle the `isDeleted` -> `null` transition when a photo is restored from trash.

### "Recently Deleted" Album

Soft-deleted photos can also be queried directly via the "Recently Deleted" smart folder, separate from `changes/zone`:

- Record type: `CPLAssetAndMasterDeletedByExpungedDate`
- Count via: `HyperionIndexCountLookup` with `indexCountID: "CPLAssetDeletedByExpungedDate"`

## Delta Data

This section shows what actual `changes/zone` responses look like for various user actions. In each case, the delta was obtained by calling `/changes/database` to detect the change (1 call) and then `/changes/zone` with a stored token to retrieve the changed records.

### 5 deletions + 2 additions

16 records returned in 1 `changes/zone` call:

| Category | Count | Details |
|----------|-------|---------|
| Soft-deleted (`isDeleted == 1`) | 10 | 5 CPLMaster + 5 CPLAsset (5 trashed photos) |
| New (no deletion flags) | 5 | 2 CPLMaster + 2 CPLAsset + 1 CPLLibraryInfo |
| Additional CPLAsset | 1 | Associated with one of the new files |

`record.deleted` was `false` on all 16 records - none of these are hard deletes.

Trashed files: `IMG_0173.DNG`, `IMG_0174.DNG`, `IMG_0176.PNG`, `IMG_0206.DNG`, `IMG_0207.DNG`

Added files:
- `IMG_0210.DNG` - photo, addedDate 2026-03-10 13:50:30 UTC
- `IMG_0211.MOV` - short video, addedDate 2026-03-10 13:50:41 UTC

Each photo produces 2 records: 1 CPLMaster (file reference with filename) + 1 CPLAsset (metadata with dates). So 7 user-visible changes produced 14 photo records + 1 library metadata update + 1 extra CPLAsset = 16 total.

### Photo edit (crop/filter)

When a user edits a photo in the Photos app, only the CPLAsset is modified. The CPLMaster (the original file) is untouched - edits in iCloud Photos are non-destructive and stored as adjustment parameters on the CPLAsset.

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

- The original file (CPLMaster) does not change, so no re-download is needed for the original
- Apple pre-renders the edited version and makes it available via `resJPEGFullRes`
- The presence of `adjustmentType` (non-null) on a CPLAsset signals that the photo has been edited
- `adjustmentCreatorCode` identifies which app made the edit (e.g., `com.apple.mobileslideshow` = Photos app)
- `customRenderedValue: 10` indicates a processed/edited image

### Restore from trash (un-delete)

When a user restores a photo from "Recently Deleted," only the CPLAsset is modified. The CPLMaster is untouched because the original file was never removed from iCloud storage during the soft-delete period.

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

- `isDeleted` goes from `1` back to `null` (not `0` - see the Field Value Asymmetry section)
- No new CPLMaster is created; the original file is still in iCloud storage
- `masterRef` still points to the same CPLMaster, so the download URL remains valid

### Hide photo

Hiding a photo modifies only the CPLAsset. The CPLMaster is untouched.

```json
{
    "recordType": "CPLAsset",
    "recordName": "11111111-2222-4333-4444-555555555555",
    "fields": {
        "isHidden": {"value": 1, "type": "INT64"},
        ...
    }
}
```

- The photo disappears from `CPLAssetAndMasterByAssetDateWithoutHiddenOrDeleted` queries (note "WithoutHidden" in the index name)
- Hidden photos are queryable via the `CPLAssetHiddenByAssetDate` record type
- Count via `HyperionIndexCountLookup` with `indexCountID: "CPLAssetHiddenByAssetDate"`

### Un-hide photo

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

`isHidden` goes to `0`, not `null`. This is different from `isDeleted`, which goes to `null` on restore (see Field Value Asymmetry). CPLMaster is untouched.

### Favorite photo

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

CPLMaster is untouched. Un-favoriting sets `isFavorite` back to `0`.

### Live Photo

A Live Photo is represented as 1 CPLMaster + 1 CPLAsset, the same as a standard photo. The video component is not a separate record - it's embedded as additional resource fields on the CPLMaster:

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

The `resOriginalRes` field contains the still image (HEIC), while `resOriginalVidComplRes` ("Video Complement to the Original") contains the motion video (QuickTime MOV). Both are separate downloadable resources on the same CPLMaster record.

| Field | Description |
|-------|-------------|
| `resOriginalVidComplRes` | Full-quality QuickTime video (Live Photo motion) |
| `resOriginalVidComplFileType` | `com.apple.quicktime-movie` |
| `resOriginalVidComplFileSize` | Size in bytes |
| `resOriginalVidComplWidth` / `Height` | Video dimensions |
| `resVidMedRes`, `resVidSmallRes` | Transcoded smaller variants |

The presence of `resOriginalVidComplRes` distinguishes Live Photos from standard photos. Standard photos have `resOriginalRes` only.

### Album membership (add photo to existing album)

Adding a photo to an existing album produces 2 records. The CPLAsset and CPLMaster of the photo itself are not modified - album membership is tracked entirely through separate records.

1. CPLAlbum (modified, updated `recordModificationDate`):

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

2. CPLContainerRelation (new membership link):

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

- `containerId` = album UUID, `itemId` = asset UUID
- `isKeyAsset` indicates whether this photo is the album's cover
- No CPLMaster or CPLAsset modifications occur

### Album membership removal

Removing a photo from an album hard-deletes the CPLContainerRelation (`deleted: true`, `recordType: null`). The CPLAlbum record is also updated with a new modification timestamp. The photo's CPLAsset and CPLMaster are not modified.

### Batch delete (15 photos)

Deleting 15 photos at once produces 30 soft-deleted records (15 CPLMaster + 15 CPLAsset). Each photo is individually represented with its own pair of records - the API does not coalesce batch operations.

### Duplicate photo (iOS "Duplicate")

Using iOS's "Duplicate" option creates a new CPLAsset that shares the same CPLMaster as the original. iCloud deduplicates at the master level, so no new binary is uploaded.

```
CPLMaster AExmplFi... -> IMG_0209.JPG (single binary)
  ^ masterRef
CPLAsset ABCD1234... (new - the duplicate)
CPLAsset CCCCCCCC... (existing - the original)
```

The delta contains 1 CPLMaster + 1 new CPLAsset (not 2 CPLMasters). `resOriginalFingerprint` on the shared CPLMaster is identical for both assets.

### Shared album add

Adding a photo to a shared album (the invite-specific kind, not iCloud Shared Photo Library) produces a different delta than adding to a personal album. In PrimarySync, only 1 record appears - a CPLAsset update with sharing metadata:

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

- No CPLContainerRelation in PrimarySync (unlike personal albums, which create one)
- No CPLAlbum update in PrimarySync
- No CPLMaster change
- The `SharedSync-{UUID}` zone is also flagged in `changes/database` - the actual shared album membership record lives there

## CPLLibraryInfo

A CPLLibraryInfo record appears in every delta whenever photos or albums change. It contains library-wide counters and metadata:

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

- The counts can be used for sanity checks (e.g., comparing `photosCount` against the number of downloaded photos)
- `lastSyncedToken` is Apple's own internal sync tracking field
- `linkedShareZoneNameList` / `linkedShareZoneOwnerList` contain references to any associated shared library zones

## Error Handling

### Invalid/Garbage Token

Sending an invalid syncToken to `/changes/zone` returns a 200 HTTP status with the error encoded in the response body:

```json
{
    "zones": [{
        "zoneID": {"zoneName": "PrimarySync"},
        "serverErrorCode": "BAD_REQUEST",
        "reason": "Unknown sync continuation type"
    }]
}
```

- The HTTP status is 200, not 4xx - the error is in `.zones[0].serverErrorCode` and `.zones[0].reason`
- This is recoverable: discard the stored token and fall back to full enumeration

### Empty Token

`syncToken: ""` behaves as caught-up (0 records, `moreComing=false`), not as full history. See the Token Edge Cases section for the distinction between empty and omitted tokens.

## SharedSync Zone (iCloud Shared Photo Library)

Apple's iCloud Shared Photo Library feature (the family-wide shared library, distinct from traditional shared albums) stores its data in a separate CloudKit zone called `SharedSync-{UUID}`. This section covers how to discover the zone, how it differs from PrimarySync, and how multi-user changes appear in the API.

### Discovery

The SharedSync zone is accessed through the `/private` endpoint, not `/shared`. The `/shared` endpoint (`/production/shared` instead of `/production/private`) returns 0 zones.

The zone name includes a UUID suffix that is unique to the shared library and must be discovered dynamically via `/zones/list`:

```json
{
    "zones": [
        {"zoneID": {"zoneName": "PrimarySync", "ownerRecordName": "_aabbccdd11223344556..."}},
        {"zoneID": {"zoneName": "SharedSync-12345678-ABCD-4EF0-9012-3456789ABCDE", "ownerRecordName": "_aabbccdd11223344556..."}}
    ]
}
```

### Supported Operations

All the same sync operations work against SharedSync:

| Operation | Supported | Notes |
|-----------|-----------|-------|
| `records/query` | Yes | Same query format, same record types |
| `changes/zone` | Yes | Full history enumeration + incremental delta |
| `HyperionIndexCountLookup` | Yes | Returns photo count for SharedSync zone |
| syncToken | Yes | Returned by `records/query`, usable in `changes/zone` |

### Differences from PrimarySync

SharedSync uses the same record types as PrimarySync (CPLAsset, CPLMaster, CPLLibraryInfo, etc.) with a few additions and differences:

1. `contributors` field - present on both CPLAsset and CPLMaster. Contains the `ownerRecordName` of the user who added the photo. This field is absent in PrimarySync records.

```json
"contributors": {
    "value": [{"recordName": "_aabbccdd1122334455667788aabbccdd", "action": "NONE"}],
    "type": "REFERENCE_LIST"
}
```

2. `CPLSharedLibraryQuota` record type - tracks storage usage per contributor. Contains `contributedQuotaInSharedLibrary`. Only present in SharedSync.

3. Scale - SharedSync can be larger than PrimarySync since it includes contributions from all family members. Example: PrimarySync ~7,300 photos, SharedSync 11,698 photos.

4. Zone name includes UUID: `SharedSync-{UUID}` vs plain `PrimarySync`.

5. `deletedBy` field - present on CPLAsset when a photo is removed from the shared library. Contains the `ownerRecordName` of the person who removed it. Only on CPLAsset, not CPLMaster.

6. Missing fields - SharedSync CPLAsset records lack some fields present in PrimarySync (e.g., `people` / `facesVersion` are absent).

### Downloads

SharedSync download URLs work identically to PrimarySync, using the same auth cookies and the same `cvws-h2.icloud-content.com` domain. No additional authentication is needed.

- With cookies: HTTP 200, full file downloaded
- Without cookies: HTTP 200 with `Content-Length: 17` (17-byte error body, 501 status in body)

### Full History Enumeration Data

First 5 pages (1,000 records) of SharedSync `changes/zone`:

| Page | Records | Breakdown |
|------|---------|-----------|
| 1 | 200 | 158 CPLAsset + 42 CPLMaster |
| 2-5 | 200 each | 200 CPLAsset each |
| Total (5 pages) | 1,000 | 958 CPLAsset + 42 CPLMaster |

Photo count via `HyperionIndexCountLookup`: 11,698. `moreComing: true` after 5 pages.

### Incremental Delta

Adding 2 photos to the shared library (1 existing photo moved to shared + 1 new photo taken directly into shared) produces:

- `changes/database` flags the SharedSync zone
- `changes/zone` returns 6 records: 2 CPLMaster + 2 CPLAsset + 1 CPLLibraryInfo + 1 CPLSharedLibraryQuota
- Token mechanics are identical to PrimarySync (2 API calls total)
- `contributors` field is present on the delta records, identifying who added each photo
- Adding an existing photo and taking a new photo directly into SharedSync produce identical delta records - they're indistinguishable in the API

### Multi-User Behavior

The following sections describe what appears in the `changes/zone` delta when another family member makes changes to the shared library. The key pattern is complete zone isolation: other people's actions only affect SharedSync, never PrimarySync.

#### Another person takes a photo into shared library

SharedSync delta: CPLMaster + CPLAsset + CPLLibraryInfo + CPLSharedLibraryQuota (4 records).

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

- The photo appears in your SharedSync delta with their user ID in the `contributors` field
- Your PrimarySync delta: 0 records (no cross-zone effect)
- The photo exists only in SharedSync, not in your PrimarySync

#### Another person removes a photo from shared library

SharedSync delta: soft delete (`isDeleted: 1`) on both CPLMaster and CPLAsset, with the `deletedBy` field identifying who did it.

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

PrimarySync delta: 0 records (complete zone isolation).

#### Another person deletes a photo vs moves to personal

These two actions produce almost identical deltas in SharedSync. The only difference is the `trashReason` field:

| Field | Move to personal | Delete |
|-------|-----------------|--------|
| `isDeleted` | `1` | `1` |
| `deletedBy` | their user ID | their user ID |
| `trashReason` | absent/null | `1` |

### trashReason Values (SharedSync)

| `trashReason` | Meaning | PrimarySync effect |
|---------------|---------|-------------------|
| `0` or absent | Moved to personal library (still exists) | New records if YOUR photo; 0 records if theirs |
| `1` | Deleted (goes to Recently Deleted -> purge after 30 days) | 0 records |

- `deletedBy` always identifies who initiated the action
- `isExpunged: 1` may accompany moves (e.g., own move-to-personal)
- PrimarySync: 0 records when another person moves/deletes; new CPLMaster + CPLAsset when you move back to personal

### Cross-Zone Behavior

When you add a photo to the shared library, records appear in both zones. When another person adds a photo, records only appear in SharedSync:

When you add a photo to Shared Library:

```
You take new photo into Shared Library:
  PrimarySync delta:  CPLMaster + CPLAsset + CPLLibraryInfo
  SharedSync delta:   CPLMaster + CPLAsset + CPLLibraryInfo + CPLSharedLibraryQuota

You add existing photo to Shared Library:
  PrimarySync delta:  CPLMaster (updated) + CPLAsset
  SharedSync delta:   CPLMaster (new) + CPLAsset + CPLLibraryInfo + CPLSharedLibraryQuota
```

When you move a photo from Shared back to Personal:

```
  PrimarySync delta:  CPLMaster (new) + CPLAsset (new) + CPLLibraryInfo
  SharedSync delta:   CPLMaster (isDeleted=1) + CPLAsset (isDeleted=1, deletedBy=you, trashReason=0, isExpunged=1)
                      + CPLLibraryInfo + CPLSharedLibraryQuota
```

When another person adds/removes a photo:

```
They take a photo into Shared Library:
  Your PrimarySync delta:  0 records
  Your SharedSync delta:   CPLMaster + CPLAsset (their contributor ID)

They move a photo from Shared -> Personal:
  Your PrimarySync delta:  0 records
  Your SharedSync delta:   CPLMaster (isDeleted=1) + CPLAsset (isDeleted=1, deletedBy=them, trashReason=0)

They DELETE a photo from Shared Library:
  Your PrimarySync delta:  0 records
  Your SharedSync delta:   CPLMaster (isDeleted=1) + CPLAsset (isDeleted=1, deletedBy=them, trashReason=1)
```

### Cross-Zone Deduplication

When syncing both PrimarySync and SharedSync, your own photos appear in both zones. The cross-zone deduplication key is `CPLMaster.recordName`, which equals `resOriginalFingerprint` across both zones. The same photo in PrimarySync and SharedSync has an identical CPLMaster `recordName`.

Other users' photos exist only in SharedSync, so cross-zone deduplication only applies to your own photos.

### Multiple Zones in changes/database

`changes/database` may report changes in multiple zones beyond PrimarySync and SharedSync:

- `PrimarySync` - main private library
- `SharedSync-{UUID}` - shared library zone
- `AppLibrarySync-com.apple.GenerativePlayground-{UUID}` - Apple Intelligence zone

## Date Field Format

Dates in CloudKit responses are milliseconds since Unix epoch (January 1, 1970 00:00:00 UTC):

```json
{
    "addedDate": {"value": 1773111052938, "type": "TIMESTAMP"},
    "assetDate": {"value": 1773111052938, "type": "TIMESTAMP"}
}
```

To convert: `1773111052938 / 1000 = 1773111052.938` -> `2026-03-10 02:50:52.938 UTC`

Date and filename fields are split across record types: `addedDate` is on CPLAsset, while `filenameEnc` is on CPLMaster. The two are linked via the `masterRef` field on CPLAsset.

Filenames are base64-encoded: `SU1HXzAyMDcuRE5H` -> `IMG_0207.DNG`

## Incremental Sync Flow

This section describes how to use the endpoints and tokens together to implement incremental sync.

### First Sync (Bootstrap)

The first sync uses the existing `records/query` flow with one addition - capturing the syncToken from the response:

1. Run existing `records/query` flow (paginate through all photos)
2. Capture `syncToken` from the `records/query` response
3. Store token persistently (e.g., in a database under key `sync_token_PrimarySync`)

No additional API calls are required. The token is already present in the response; it just needs to be captured.

### Subsequent Syncs

On subsequent runs, the stored token is used to check for and retrieve only the changes:

```
Step 1:  Load stored token
Step 2:  POST /changes/database {syncToken: db_token}
         -> If zones is empty: nothing changed, done
         -> If PrimarySync in zones: proceed to Step 3
Step 3:  POST /changes/zone with stored zone token
         -> Page through moreComing until caught-up
         -> Filter for CPLAsset + CPLMaster records
         -> Process additions/modifications/deletions
Step 4:  Store new caught-up token
```

### API Call Comparison

| Scenario | Full scan | Incremental |
|----------|-----------|-------------|
| No changes | ~75 calls (7,300 photos / 200 per page x 2 records) | 1 call (`changes/database` -> 0 zones) |
| 1 photo added | ~75 calls | 2 calls (`changes/database` + `changes/zone` -> 2 records) |
| 100 photos added | ~75 calls | 2 calls (`changes/database` + `changes/zone` -> 200 records) |

## Struct Suggestions

These Rust struct definitions correspond to the request/response shapes described above.

### QueryResponse (existing, add sync_token)

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

### changes/zone Types

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

### changes/database Types

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
