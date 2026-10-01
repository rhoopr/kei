# Media fixture corpus

The 13 media files in `media-manifest.json` cover transfer and metadata without
Apple credentials, external downloads, or encoders during tests. Their total
size is 7,959,356 bytes. Four existing structural regression seeds add 2,747
bytes. The enforced budget for all 17 files is 8 MiB.

## Coverage

| Fixtures | Purpose | Production-path test |
| --- | --- | --- |
| `media/pattern.jpg`, `.png`, `.heic`, `.avif`, `.mov`, `.mp4`, `.dng` | Valid, independently encoded formats, no source location or personal content | `bundled_media_download_finalize_reopen_and_second_sync` |
| `media/metadata.jpg` | Controlled capture time, orientation 6, rating 4, Unicode description, GPS at 0,0 | `bundled_media_download_finalize_reopen_and_second_sync` |
| `media/apple-live.heic`, `media/apple-live.mov` | Authentic Apple still, HEVC motion, silent PCM audio, three timed metadata tracks, matching pairing identifiers | `bundled_live_photo_modes_preserve_pair_and_companion_naming` |
| `media/icloud-still.heic`, `media/apple-live.heic`, `media/pattern.heic`, `media/pattern.avif`, `apple-hdr-gainmap.heic` | XMP before publication, original/local checksum separation, unchanged non-XMP items | `bundled_heif_prepublication_xmp_preserves_media_and_checksum_roles` |
| `apple-hdr-gainmap.heic`, `white_1x1.avif` | Existing independent Apple HDR/tiled and AVIF layouts | HEIF owner tests and `bundled_media_download_finalize_reopen_and_second_sync` |
| Derived damaged JPEG bytes | Malformed and truncated HTTP responses, durable retry after reopening SQLite | `bundled_invalid_download_retains_retry_evidence_then_recovers_after_restart` |

The [live migration map](live-migration.md) lists filtering, naming, metadata,
import, and failure replacements. Additional fixture tests prove interrupted
HTTP work survives restart and conflicting local media is not overwritten.

Each media transfer test uses controlled provider records, real enumeration
and planning, local HTTP, file-backed SQLite, and the normal publication path.
Successful cases reopen SQLite and run an unchanged second sync. Exact HTTP
request counts reject repeat downloads. Invalid response cases retain failure
state before recovery and leave no temporary files after success.

`tests/media_fixtures.rs` verifies file sizes, SHA-256 hashes, the corpus budget,
manifest completeness, packaged licences and recipes, and the shared Live Photo
identifier. A deliberate
same-size mutation proves the integrity check fails on changed bytes.
Malformed and truncated variants are derived in tests; extra binary copies
are not needed.

The synthetic DNG is a valid 32x32 uncompressed Bayer image. LibRaw, through
rawpy 0.27.1, decoded its mosaic and rendered RGB independently of tifffile.
It tests RAW transfer without adding a 31 MB camera file. It is not Apple
ProRAW. The synthetic movie is not an authentic Live Photo.

## Provenance and licences

Synthetic files are generated for kei and use the repository MIT licence.
`media/encoders.json` records the independent encoder versions.

The flower Live Photo comes from Rhet Turnbull and the OSXPhotos contributors:

- Repository revision: `cbe5a1d1df3719caaf07d1940ce0b32a6b85c191`.
- Original [still](https://github.com/RhetTbull/osxphotos/blob/cbe5a1d1df3719caaf07d1940ce0b32a6b85c191/tests/test-images/IMG_1853.HEIC) and [movie](https://github.com/RhetTbull/osxphotos/blob/cbe5a1d1df3719caaf07d1940ce0b32a6b85c191/tests/test-images/IMG_1853.mov).
- The [test attribution](https://github.com/RhetTbull/osxphotos/blob/cbe5a1d1df3719caaf07d1940ce0b32a6b85c191/tests/README.md#attribution) places its test imagery under CC BY 2.0. A copy is in `LICENSE-osxphotos-media`.
- These are modified copies: GPS and location-accuracy tags were removed,
  the still's photo identifier was removed, both pairing identifiers were
  replaced with one synthetic identifier, and PCM audio samples were zeroed.
- `media/sanitization.json` records original hashes and preservation checks.
  The flower scene was visually reviewed. The distributed audio decodes
  entirely to silence. No GPS, location, or camera serial tags remain.
- All video and timed-metadata packets retain their exact payloads, sizes,
  timestamps, and durations. The still decodes to identical pixels. Neither
  image nor video was transcoded.

These are Apple camera/Photos fixtures. Their exact iCloud delivery history is
not documented, so they are not labelled as iCloud downloads. Provider/API
compatibility remains the responsibility of live tests. Byte-format coverage
does not depend on an unverifiable claim about a file's transport history.

The existing HDR sample is from
[`johncf/apple-hdr-heic`](https://github.com/johncf/apple-hdr-heic/blob/286d5b094f0bd901a35aa0b8582746aedcb32bb4/tests/data/hdr-sample.heic),
under the MIT terms in `LICENSE-apple-hdr-gainmap`. Its technical Apple photo
identifier is retained; it has no GPS or serial-number tags. The existing
1x1 AVIF is from
[`libavif`](https://github.com/AOMediaCodec/libavif/blob/cbb391c194ee15cf9607e517c269ed99e6bf0197/tests/data/white_1x1.avif),
under `LICENSE-libavif-white-1x1`.

`sample.heic` remains a separate legacy fixture for existing owner tests.
This corpus does not change its bytes or claim additional provenance for it.

### Maintainer-contributed iCloud sample

`media/icloud-still.heic` was downloaded from the maintainer's iCloud Photos
shared library with kei on 2026-09-27. The maintainer confirmed ownership and explicitly
authorized sanitized samples under MIT. `LICENSE-icloud-media` contains the
licence. This is an iCloud-delivered Apple HEIC, not a synthetic encoding.

The maintainer selected this Friday-evening sunset. It shows the sky, trees,
and a yard, with no people, documents, or visible addresses.
Sanitization removes Apple maker notes, identifiers, and primary XMP
(including region tags and capture dates). It replaces EXIF capture timestamps
with a fixed date. Technical auxiliary XMP values remain unchanged, as verified by ExifTool. The HEVC image data, auxiliary
images, orientation, and HDR metadata remain. FFmpeg decoded the original and
sanitized sample to identical pixels. The source hash and checks are in
`media/icloud-sanitization.json`; original filenames and account metadata are
not distributed. The pinned sanitizer needs the privately retained original.

## Reproduction

Normal tests read the committed bytes. Regeneration is a maintainer operation
and needs a new output directory. Do not regenerate fixtures during tests.

Use Pillow 12.3.0, pillow-heif 1.8.0, numpy 2.5.3, and tifffile 2026.9.20 in
an isolated Python environment. The HEIC wheel used libheif 1.23.4 and x265
4.2+1-e444744. Pillow used libavif 1.4.2. Generation also used FFmpeg 8.1.1 and
ExifTool 13.59.

```sh
python3 scripts/fixtures/generate.py /tmp/kei-new-media --exiftool /path/to/exiftool
python3 scripts/fixtures/sanitize_live_photo.py /path/to/pinned-originals /tmp/kei-new-live-photo --exiftool /path/to/exiftool
python3 scripts/fixtures/sanitize_icloud_still.py /path/to/contributed-original.heic /tmp/kei-new-icloud --exiftool /path/to/exiftool
```

The sanitizer rejects source hashes other than the pinned originals. It
checks unchanged non-audio packets, silent decoded audio, identical decoded
still pixels, retained pairing/timed metadata, and removal of location tags.
Review new bytes and encoder changes before updating manifest hashes.

### Independent rewrite audit

To keep the actual HEIF pipeline outputs for an independent decoder, run:

```sh
KEI_FIXTURE_INSPECT_DIR=/tmp/kei-rewritten-media cargo test --all-features --lib bundled_heif_prepublication_xmp_preserves_media_and_checksum_roles -- --test-threads=1
```

The five output paths match their paths under `tests/data/`. FFmpeg 8.1.1
decoded each original and rewritten file to identical RGB24 pixels. ExifTool
13.59 read rating 5 from each rewritten file. Repeat those checks after writer
or fixture changes. For example, compare these hashes and check the rating:

```sh
ffmpeg -v error -xerror -i tests/data/media/apple-live.heic -frames:v 1 -pix_fmt rgb24 -f hash -hash sha256 -
ffmpeg -v error -xerror -i /tmp/kei-rewritten-media/media/apple-live.heic -frames:v 1 -pix_fmt rgb24 -f hash -hash sha256 -
exiftool -s -s -s -XMP-xmp:Rating /tmp/kei-rewritten-media/media/apple-live.heic
```

These external tools are for maintainer audits, not normal test runs.

## Source packaging

`just test packaging` checks the release archive and runs
`scripts/fixtures/check-package.sh`. The latter extracts a real Cargo source
package and runs the integrity and production-path tests in optimized builds,
with all features and without default features. Cargo includes fixtures,
licences, the manifest, and generation recipes in the source package.

Cargo excludes the nested `fuzz/` package. Four existing HEIF owner tests'
inputs are therefore copied to `heif-rewrite/`, with hashes in the manifest.
The package check compares these copies with their fuzz seeds before packing.
These small structural cases complement the independent media; they are not
valid-camera-media samples. No existing regression test is removed.

Media is loaded only by test code. It is not embedded in the production
executable. The runtime image receives the compiled executable, not this
corpus. A package check uses committed fixture bytes and never fetches media
or runs an encoder.
