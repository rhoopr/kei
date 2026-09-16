# Metadata output

kei captures provider metadata in SQLite without enabling local metadata writes.
Embedded output and XMP sidecars are off by default. Enable only the outputs you
want in [example.config.toml](../example.config.toml).

```toml
[metadata]
xmp_sidecar = true
```

Sidecars leave media bytes unchanged. Embedded writes change local bytes and
record their resulting checksums. Back up originals before enabling them.

## Capture, output, and repair

Automatic capture repair processes at most 500 stale assets per selected library
per cycle. One-shot sync handles one bounded batch. Watch/service schedules a
follow-up after at most 60 seconds only when clean progress leaves work remaining;
failed or stalled batches use normal cadence. A cycle can have local work even
when Apple reports no provider changes.

Status separates stale catalogue capture from pending configured-output rewrites.
The report uses an optional integer `metadata_capture_revision`, omitted when
unavailable, and integer `metadata_capture_refreshed`, `metadata_capture_failures`,
and `metadata_capture_remaining` counts. A metadata writer must be enabled before
its output retry can run. Captured data alone does not authorize local writes.

An explicit [metadata refresh](backup-maintenance.md#refresh-catalogue-and-configured-outputs)
revisits the selected libraries without downloading media again. Explicit
capture-timestamp replacement, truncated-media replacement, and path
reconciliation each have separate consent and safety rules.

## Dates and format support

Capture dates use Apple's usable per-asset offset; otherwise they use host-local
rendering. Date-only filters compare that calendar date. Datetime and relative
interval filters compare an instant. Capture/addition milliseconds are retained
in the catalogue and supported sidecar output, not newly written as native EXIF
subseconds. See [compatibility](v0.24-upgrade.md#fractional-timestamps).

Ordinary datetime output preserves native timestamps. When safe, it adds a
matching offset or fills a missing timestamp. Replacing an existing timestamp
requires the explicit capture-repair command and its evidence checks.

| Output | Capability |
| --- | --- |
| Native JPEG/TIFF EXIF | Available without the `xmp` Cargo feature |
| JPEG/PNG/TIFF embedded XMP | Adobe XMP Toolkit with `xmp` |
| MP4/MOV embedded XMP | Adobe XMP Toolkit with `xmp`; not a promise of native movie-tag rewriting |
| HEIC/HEIF/AVIF embedded metadata | Byte-preserving item-map XMP writer with `xmp`; supported layouts only |
| RAW and other media | XMP sidecars with `xmp`; no general embedded-write promise |

`metadata.set_exif_datetime` includes supported HEIF XMP datetime output with
`xmp`; it is not limited to JPEG/TIFF. The default Cargo feature enables the
capability, not automatic mutation. Unsupported formats can still have sidecars.

### HEIF safety and limits

HEIC/HEIF/AVIF embedding is advanced opt-in support. The production writer edits
the primary image's XMP item map and preserves encoded image bytes and auxiliary
gain-map, depth, and matte content. The old typed `mp4-atom` writer is test-only.
Tone-map scope must be proven by item relationships. Ambiguous relationships,
conflicting XMP ownership, external item data, and unsupported layouts fail
without replacing the media.

A media download can succeed while its metadata write fails and remains pending.
Inspect metadata failures, not only downloaded/failed counts. This safe writer
prevents the old corruption path; it does not detect or restore every historically
damaged container. Capture-timestamp repair is not general HEIF repair.

The writer still buffers whole files and can amplify memory use under concurrent
writes. No universal memory ceiling is claimed. See the
[validation record](v0.24-validation.md) and issues
[#761](https://github.com/rhoopr/kei/issues/761) and
[#817](https://github.com/rhoopr/kei/issues/817).

## Sidecar ownership and convergence

kei updates fields it owns and can clear obsolete managed fields. It preserves
custom/unowned properties. Existing values do not always win: ownership markers
control managed replacements and clearing. An unreadable, malformed, or
concurrently changed packet remains untouched with durable retry evidence;
there is no fallback that discards it for a fresh packet.

Album and people groupings converge on supported rewrites, including membership
additions/removals. Album membership is available on the first write. Recorded
output paths retry independently. Live Photo stills and movies keep their own
rendition dimensions and duration. This does not claim ownership of every
historical copy.

Untouched stale sidecars have no automatic output-revision sweep; that remains
[#799](https://github.com/rhoopr/kei/issues/799). Path reconciliation copies an
existing valid packet rather than regenerating it.

### GPS accuracy provenance

Exporting native horizontal accuracy alongside provider coordinates requires
matching native latitude/longitude and verified original-source SHA-256 evidence.
A coordinate match manufactured by kei's own GPS embedding is insufficient.
Rounding differences can omit accuracy; there is no geographic tolerance.

New verified downloads can establish `source_checksum` before metadata writes.
Older, adopted, reconciled, or subsequently rewritten bytes may lack usable
evidence, so valid-looking accuracy can be omitted conservatively. Historical
local/download checksums are not substituted. Native/unowned metadata remains.
A supported rewrite clears an obsolete accuracy field only when kei owns it.

Unknown evidence differs from retryable source I/O failure. A source-read failure
can publish current provider fields while retaining source-field retry evidence;
readable unsupported or malformed source metadata permits a provider-only packet.
This rule does not expand GPS speed/time or movie-field policy.

## Guarded publication

Metadata writers create unique temporary files exclusively, validate stable
inputs and approved replacement fingerprints, preserve permissions, and fsync
before guarded publication. Media replacement uses atomic exchange where
applicable. Sidecars have their own existing-packet ownership and byte checks;
reports do not share all of these rules. A changed input cannot authorize a
blind rename over user data. Ambiguous entries stay available for inspection.
