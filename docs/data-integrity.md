# Data integrity

kei's first invariant is that user data is sacred. A photo is either fully present and valid on disk, or absent. There's no in-between state where a corrupt or truncated file gets recorded as downloaded.

This page covers the layers kei stacks to make that hold, the residual risks the iCloud API can't help us close, and how to detect divergence after the fact.

## Atomic writes

Every download streams bytes to a `.part` file and only renames into place once verification passes. If kei crashes, gets killed, or loses the network mid-download, the final path either doesn't exist yet or is the previous good copy. There's no half-written file masquerading as complete.

The same pattern covers kei's own state files. The session, credential, and config writers go through tmp + fsync + rename. The state DB uses SQLite WAL with `synchronous=NORMAL`, so a crash mid-transaction rolls back cleanly.

`.part` files survive across runs. The next sync picks up where the previous one left off via HTTP Range requests. A `.part` older than an hour gets discarded as stale rather than appended to.

## Download verification

Three independent checks gate the rename:

1. **Content-Length.** The number of bytes received matches the header.
2. **Magic-byte / sentinel.** The first 16 bytes match the expected file family (JPEG, HEIF, MP4, etc.) and don't look like an HTML or JSON error page.
3. **Expected size.** The byte count matches the size the iCloud API claimed for this asset.

If any check fails, the `.part` is removed and the download is retried with a fresh CDN URL. The final path is never written.

After the rename, kei computes a SHA-256 of the bytes on disk and stores it as `local_checksum` in the state DB. This is a record of what kei wrote, not a comparison against a published hash (Apple doesn't expose one - see below).

## What Apple doesn't give us

CloudKit's asset metadata includes a `fileChecksum` field, but it's an MMCS compound signature - a hash of hashes covering the asset's storage layout, not the file bytes themselves. There's no SHA-256, MD5, or other content hash kei can verify the downloaded body against.

The practical consequence: if the CDN ever returns bytes that pass all three layered checks but aren't byte-for-byte identical to what Apple meant to deliver, kei will write the corrupt file, record its SHA-256, and mark it downloaded. Future incremental syncs won't re-fetch it.

The shapes of attack this leaves open:

- A proxy that mangles bytes mid-stream while updating the Content-Length header it forwards.
- A cosmic-ray flip on a multi-MB transfer where the magic bytes still match.
- A truncation where the truncating party also rewrites Content-Length to match.

These are rare. The layered checks catch every realistic failure mode kei has actually seen in the wild. But "rare" isn't "impossible," and the residual risk is documented here so anyone running kei on infrastructure they don't trust knows where to set their expectations.

## Detecting divergence after the fact

`kei verify --checksums` re-reads every downloaded file, computes a fresh SHA-256, and compares it against `local_checksum` in the state DB. Any mismatch surfaces as a verify failure with the asset ID and on-disk path.

```sh
kei verify --checksums
```

This catches three classes of problem:

- The file on disk has been modified since kei wrote it (filesystem corruption, accidental edit, ransomware).
- `local_checksum` itself is corrupt (database damage).
- The hash routine produces a different digest than it did when the file was first downloaded (regression in kei).

It does not catch corruption that was already present when kei first computed the hash. For that case, the fixes are operational: run `kei sync --retry-failed` after any incident that you suspect corrupted a download in flight, or use `kei reset state` and re-sync from scratch if you have a strong reason to distrust the existing files.

## Idempotency

Running the same sync twice produces the same result. `kei sync` is safe to interrupt with Ctrl-C and re-run. The state DB tracks every asset by `(id, version_size)`, so a half-finished sync resumes cleanly without re-downloading anything that already landed and without losing anything that was in flight.

The sync token only advances on full success. A partial failure holds the token back so the next run replays the failed assets via the delta endpoint instead of falling back to a full enumeration.

## What kei never does

- Modify or delete user files on disk without an opt-in flag. Every file-modifying operation defaults to off.
- Treat a remote deletion as a local delete. There's no path that would let an iCloud-side change remove a photo from your disk.
- Commit a sync as successful if any asset failed. The exit code reflects cumulative failures across all cycles.
- Swallow errors silently. If kei reports a successful sync, it succeeded. Anything that fails gets logged with structured context and counted in the cycle stats.

## Reporting integrity issues

Found a case where kei wrote a corrupt file or marked a failed download as successful? File an issue with the `data-integrity` label. Include the structured log lines around the failure (`tracing` output at `info` level or higher), the asset ID, and the kei version. Data integrity bugs jump the queue.
