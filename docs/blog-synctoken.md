---
title: "10 Years Later, I Reverse-Engineered iCloud's syncToken by Brute Force"
description: "A decade old TODO in pyicloud: 'Does syncToken ever change?' I brute-forced Apple's iCloud Photos API to find out. The answer makes photo sync 75x more efficient."
author: Rob Hooper
date: 2026-03-11
---

# 10 Years Later, I Reverse-Engineered iCloud's syncToken by Brute Force

In the source code of pyicloud, the Python library behind many open-source iCloud Photos tools, there's a comment that's been sitting untouched for roughly a decade:

```python
# TODO: Does syncToken ever change?
```

Beneath it, four lines of commented-out code that would've captured and used the token Apple's servers return on every API response. Nobody ever enabled the code or answered the question, and understandably so - Apple doesn't document this API, and figuring out the answer requires sustained brute-force testing against production servers. Hard to justify when the tool already works.

[iCloud Photos Downloader](https://github.com/icloud-photos-downloader/icloud_photos_downloader) (icloudpd) is a remarkable project. Actively maintained for years, but unfortunately currently looking for a maintainer. It runs on everything from laptops to NAS boxes, handles authentication edge cases most people never think about, and has reliably backed up millions of photos. I use it as the foundation for [icloudpd-rs](https://github.com/rhoopr/icloudpd-rs), a ground-up Rust rewrite, and the Python codebase was an invaluable reference for understanding iCloud's authentication flow, photo metadata, and download pipeline.

I wanted to take the sync protocol further, though, so I went through the exercise of brute-forcing Apple's private CloudKit API, running hundreds (thousands?) of calls against a real library, and writing down everything I found. The syncToken does change, and using it properly cuts sync from ~75 API calls to 1.

---

## What's Not Documented

Apple does publish documentation for CloudKit Web Services, including [REST API endpoints](https://developer.apple.com/library/archive/documentation/DataManagement/Conceptual/CloudKitWebServicesReference/index.html) like `changes/zone`, `changes/database`, `records/query`, and `zones/list`. The [CloudKit JS reference](https://developer.apple.com/documentation/cloudkitjs/cloudkit.recordzonechanges/synctoken) describes syncToken as a property that "identifies a point in the zone's change history." The generic protocol is documented.

What's *not* documented is anything specific to iCloud Photos. iCloud Photos uses CloudKit as its backing store, but Apple publishes nothing about how it organizes data within CloudKit. Each photo in iCloud is represented by two linked records: a "CPLAsset" (metadata like dates, album membership, and flags) and a "CPLMaster" (the actual file reference with download URLs and checksums). There are also records for albums, album membership links, and library-level counters, but the schemas for all of these are completely undocumented. The same goes for the deletion model, the field naming conventions, and any of the edge cases in how the generic CloudKit endpoints behave when pointed at a real photo library.

The only "documentation" for the Photos-specific layer is the source code of tools that have reverse-engineered enough to download photos: pyicloud, icloud-photos-sync, and a handful of others. The closest thing to systematic API documentation is steilerDev's [icloud-photos-sync](https://icps.steiler.dev/dev/api/), which includes a Postman collection and documented authentication flow, but covers auth only and doesn't touch change tracking. None of them use syncToken — icloud-photos-sync implements its own diffing algorithm, and tools like osxphotos bypass the API entirely by reading the local macOS Photos database. Not surprising, given what's involved in answering the question. So I did it empirically.

---

## The Testing Methodology (And the Rate Limits)

There's no sandbox for Apple's private API, so every test ran against a real iCloud account with a real photo library. Too many calls too quickly gets you HTTP 503 responses or temporary session blocks.

1. **Observe and probe.** The standard `records/query` response includes a `syncToken` field. If it's a change bookmark, there should be an endpoint that accepts it. Apple's public CloudKit has `/changes/zone` — the private API does too, and it accepts the token.

2. **Brute force the semantics.** Each property of the token required its own tests. Is it deterministic? Idempotent? Does it support random access? Survive session refresh? Work across endpoints? Every test means API calls against rate limits and careful comparison of response bodies.

3. **Mutate and observe.** Delete 5 photos, add 2, check the delta. Edit a photo, restore one from trash, take a Live Photo, check the delta. Hide one, favorite one. Every mutation changes library state irreversibly, so I had to plan test sequences carefully. Batch operations (deleting 15 photos at once) were important because I needed to know whether the API coalesces them. It doesn't — each photo gets its own set of records.

4. **Document.** Write down every finding immediately — actual response fields, record counts, and token values, not interpretations.

Some tests had to be repeated after edge cases invalidated earlier assumptions. One thing I didn't expect: during full history enumeration, pages 27 through 96 returned zero records with `moreComing: true`. Sixty-nine consecutive empty pages (nice), apparently from the API walking through internal log segments that had been compacted. A naive implementation would bail out early and miss the rest of the data.

---

## What I Found

The full technical specification is in the [syncToken Reference](https://github.com/rhoopr/icloudpd-rs/blob/main/docs/synctoken-reference.md). Here's a summary.

### The Token Is a Zone-Wide Change Bookmark

CloudKit organizes data into "zones" - isolated containers within a database. iCloud Photos keeps your personal library in a zone called `PrimarySync`. A syncToken is a position in that zone's internal mutation log: every photo addition, deletion, edit, hide, favorite, or album change advances the log, and the token says "give me everything since here."

It's a persistent, deterministic, replayable bookmark that you can store on disk and use days later, even after re-authenticating - tokens survive session refresh. It supports random access too: I saved a token from page 2, continued to page 10, then jumped back to the page 2 token and got the same data as the first time through.

The detail that makes the whole scheme work at zero cost: the syncToken that `records/query` returns is the *exact same format* as the token used by `/changes/zone`. They're interchangeable. I took a token from a `records/query` response and passed it directly to `/changes/zone`, and it accepted it without error and returned valid change records. So the first full scan - the one every tool already does - produces a usable change-tracking token for free. The token has been sitting in every `records/query` response all along, but without knowing what it does, there was no reason to capture it.

### Three Endpoints

`/changes/database` answers one question: has anything changed in any zone? If nothing changed, you get back an empty array, and you're done in a single call. This is what a background sync process should check on every poll interval before doing anything else.

`/changes/zone` is the workhorse. Give it a token, get back every record that changed since that position. It pages through results with `moreComing` flags, 200 records at a time. A few things for implementers to watch out for: the server-side record type filter is unreliable on this endpoint, so you have to filter client-side. And each page returns a new, unique syncToken that works as a resume point, which means crash recovery comes for free.

`/records/query` is what existing tools use for full enumeration. It has its own pagination mechanism (`continuationMarker`), completely separate from syncToken. Both can appear in the same response - one is a page cursor, the other is a zone-level change bookmark.

### Deletion Is Two Different Things

This will bite anyone implementing sync without knowing about it.

When a user deletes a photo in the Photos app, it goes to "Recently Deleted" (a 30-day trash). In the API, the record is *modified*, not removed. The top-level `deleted` flag on the record is `false`. The actual signal is buried in a field inside the record: `fields.isDeleted.value == 1`. All the photo's metadata, download URLs, everything is still there - the record just has this flag flipped. In my testing, deleting 5 photos produced 10 modified records (a CPLMaster and CPLAsset for each photo), all with `deleted: false` at the record level. If you only check the record's `deleted` flag, you miss every user-initiated deletion.

After 30 days (or if the user manually empties "Recently Deleted"), records are purged for real. Now `deleted` is `true`, the record type is `null`, and all fields are gone - you can't even tell what kind of record it was. Apple retains a lot of this purge history: 15,618 hard-deleted records out of 42,787 total in my ~7,300 photo library, or about 36% of the zone's history.

Then there's a field value inconsistency that makes generic handling tricky. The `isDeleted` field uses `null` to mean "not deleted," while the similar `isHidden` and `isFavorite` fields use `0` to mean "not hidden" / "not favorited." Restoring a photo from trash sets `isDeleted` back to `null`, not `0`. Un-hiding a photo sets `isHidden` to `0`, not `null`. If you write one piece of code to handle all these boolean-like fields the same way, it'll get the edge cases wrong.

One more gotcha for implementers: sending an empty string as the syncToken and omitting the syncToken field entirely produce completely different behavior. Empty string means "I'm caught up, what's new?" and returns nothing. Omitting it means "start from the beginning" and triggers a full history enumeration. It's easy to conflate the two.

### The Shared Library Is a Separate Zone

Apple's iCloud Shared Photo Library (the family-wide one, not traditional shared albums you create and invite people to) lives in its own CloudKit zone called `SharedSync-{UUID}`. The UUID is unique per shared library and must be discovered dynamically via the `/zones/list` endpoint.

One thing that tripped me up: the zone lives under the `/private` endpoint, not `/shared`, which returns nothing.

The same sync mechanics apply - same endpoints, same token behavior, same record types. SharedSync adds a `contributors` field to track who added each photo, and `deletedBy` to track who removed it.

I tested the cross-zone behavior with another family member making changes in real time. When *you* add a photo to the shared library, records appear in *both* your PrimarySync (personal) zone and the SharedSync zone. When *someone else* adds a photo, records appear only in SharedSync - your personal zone's delta is empty, and zone isolation is complete for other people's actions.

Removal from the shared library has a subtle distinction controlled by a single field. When someone removes a photo, `isDeleted` is `1` in both cases - whether they moved it back to their personal library or actually deleted it. The `trashReason` field is what separates the two: absent means "moved to personal library, still exists somewhere," while `1` means "actually deleted, going to trash for 30 days." Without checking `trashReason`, those two operations look identical in the API response.

My SharedSync zone had 11,698 photos, larger than the 7,300 in PrimarySync, since it includes contributions from all family members. When syncing both zones, your own photos appear in both, so you need to deduplicate by file fingerprint. Other people's photos only exist in SharedSync, so there's no dedup concern for those.

---

## The Numbers

For a library of ~7,300 photos:

| Scenario | Without syncToken | With syncToken |
|----------|-------------------|----------------|
| Nothing changed | ~75 API calls | 1 call |
| 1 photo added | ~75 API calls | 2 calls |
| 5 deletions + 2 additions | ~75 API calls | 2 calls (16 records) |
| 100 photos added | ~75 API calls | 2 calls |

The "without" column is what any full-enumeration approach does: page through all photos to see what's there. Tools like icloudpd have smart workarounds - `--until-found` stops after encountering N consecutive already-downloaded photos, and `--recent` limits to the N most recent. These are effective for common cases, but they start from the top and work backward, and by design they can't detect deletions or edits. syncToken solves a different problem at a different layer.

For watch mode (running continuously and polling for changes on an interval), the difference compounds. Every poll interval that finds no changes drops from ~75 API calls to 1. Over a day with hourly polling, that's ~1,800 unnecessary API calls down to 24.

---

## Implementation

The syncToken system is fully implemented in [icloudpd-rs](https://github.com/rhoopr/icloudpd-rs). Tokens are persisted in a SQLite database. The first run does a standard full scan and captures the token at zero additional cost since it's already in the response. Every subsequent run loads the stored token, checks `/changes/database`, and only hits `/changes/zone` if something actually changed.

Crash recovery works because tokens are deterministic and replayable - if the process dies mid-pagination, the last persisted token is still valid and you resume from there. If a stored token has expired or been invalidated, the API returns a `BAD_REQUEST` error, which is detectable and triggers an automatic fallback to a full scan.

The implementation handles both PrimarySync and SharedSync zones with independent per-zone tokens.

---

## Why This Hasn't Been Done Before

That TODO is old, but the question it asks is genuinely hard to answer.

The Photos-specific layer is undocumented. Apple documents the generic CloudKit Web Services protocol, but nothing about how iCloud Photos uses it - the record types, field schemas, deletion semantics, and zone behaviors are all specific to iCloud Photos and not described anywhere. You can only figure them out by testing against a real account, spending real API calls, and hoping you don't hit a rate limit in the middle of a multi-step test sequence.

The existing tools made reasonable design choices. icloudpd has a deliberate stateless architecture: it looks at the filesystem to decide what to download, which keeps things simple and predictable. That design has served thousands of users well. Adding syncToken support means introducing persistent state (a database), which is a meaningful architectural shift.

The token behavior also has non-obvious edge cases that could silently corrupt a sync implementation. The empty-string-vs-omitted distinction. Separate, incompatible token namespaces for database-level and zone-level queries. Sixty-nine consecutive empty pages during enumeration (nice). Inconsistent null-vs-zero conventions on boolean-like fields. A server-side record type filter that silently returns wrong results on one endpoint but works fine on another. You don't find these without systematic testing.

---

## The Reference

I've published the complete technical specification as a standalone document: **[iCloud Photos CloudKit API: syncToken Reference](https://github.com/rhoopr/icloudpd-rs/blob/main/docs/synctoken-reference.md)**. It covers the endpoints, token properties, record types, and edge cases, with the test evidence behind each finding.

As far as I can tell, this is the only public documentation of the iCloud Photos-specific layer on top of CloudKit - the record types, field schemas, deletion semantics, shared library zone behavior, and the practical edge cases you hit when applying CloudKit's change-tracking protocol to a real photo library. The question in that decade-old TODO has an answer now.

---

*[icloudpd-rs](https://github.com/rhoopr/icloudpd-rs) is MIT-licensed and in active development. The core sync engine, SQLite persistence, and SharedSync support are implemented. Contributions welcome.*
