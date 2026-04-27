# Fuzzing

Coverage-guided fuzz harnesses for the parsers in kei that consume
attacker-controllable input: CloudKit JSON from Apple, base64-wrapped
binary plists in `*Enc` metadata, the iCloud auth response shapes,
filename / path sanitization on network-supplied strings, the HEIC atom
tree on every downloaded HEIC, and the XMP packet inside it (which goes
through Adobe's vendored C++ XMP Toolkit). Plus user-supplied TOML
config and corruptible on-disk state-DB enum strings.

Run by hand. Not part of `just gate` or CI. The point is to leave a
target running locally for minutes-to-hours when poking at a parser, or
to repro a specific input.

## Prereqs

```sh
rustup install nightly
cargo install cargo-fuzz
```

cargo-fuzz needs nightly because libfuzzer-sys uses unstable sanitizer
flags. Production kei still builds on stable; the `fuzz/` crate is its
own package and isn't part of any workspace.

## Run

```sh
just fuzz list                          # what's available
just fuzz run cloudkit_json             # 60s default
just fuzz run cloudkit_json 600         # 10 minutes
```

Or directly:

```sh
cargo +nightly fuzz run cloudkit_json -- -max_total_time=60
```

A discovered crash lands in `fuzz/artifacts/<target>/crash-<hash>` with
a one-line repro:

```sh
cargo +nightly fuzz run <target> fuzz/artifacts/<target>/crash-<hash>
```

Corpus entries accumulate in `fuzz/corpus/<target>/`. Both directories
are gitignored.

## Targets

| Name | What it fuzzes | Source |
|------|----------------|--------|
| `cloudkit_json` | every CloudKit response type (`ZoneListResponse`, `QueryResponse`, `Record`, `ChangesDatabaseResponse`, etc.) via `serde_json::from_slice` | `src/icloud/photos/cloudkit.rs` |
| `enc_decoders` | `decode_string`, `decode_keywords`, `decode_location`, `decode_location_with_fallback` - both the JSON-shape path and the bplist-via-base64 path | `src/icloud/photos/enc.rs` |
| `paths_sanitization` | `clean_filename`, `sanitize_path_component`, `expand_album_token`, `add_dedup_suffix`, `strip_python_wrapper`, `remove_unicode_chars` | `src/download/paths.rs` |
| `heif_atoms` | `extract_xmp_bytes` and `is_heif_content` - mp4-atom walks the ISO-BMFF box tree on attacker-controlled HEIC bytes | `src/download/heif.rs` |
| `xmp_packet` | `XmpMeta::from_str` directly - drives Adobe's vendored XMP Toolkit (C++ via FFI) on arbitrary UTF-8. cargo-fuzz only instruments Rust code, so libfuzzer is blind to coverage inside the C++ and this target is effectively dumb-fuzzed (ASan still catches memory bugs). A starter seed lives at `fuzz/seeds/xmp_packet/minimal.xmp`. | `xmp_toolkit` crate |
| `heif_xmp_probe` | full `probe_exif_heif` pipeline: extract XMP from HEIC bytes, then parse it through xmp_toolkit | `src/download/heif.rs` + `xmp_toolkit` |
| `toml_config` | `TomlConfig` deserializer + custom field deserializers (`folder_structure`, `RecentLimit`) + `deny_unknown_fields` boundary checks | `src/config.rs` |
| `auth_responses` | `SrpInitResponse`, `AccountLoginResponse`, `TwoFactorChallenge` (with custom deserializer for `fsa_challenge` + `service_errors`) | `src/auth/responses.rs` |
| `photo_asset_from_record` | `PhotoAsset::from_records`: drives `decode_filename`, `resolve_item_type`, `extract_versions`, and `metadata::extract` in one go. Splits input on the first NUL into two CloudKit `Record` JSON values | `src/icloud/photos/asset.rs` |
| `state_enums_from_str` | inherent `from_str` parsers on `VersionSizeKey`, `AssetStatus`, `MediaType` - inputs come from a sqlite state DB on disk that could be replayed, hand-edited, or corrupted | `src/state/types.rs` |

## Findings

`heif_atoms` and `heif_xmp_probe` both find an unbounded allocation in
`mp4-atom` within seconds: a 110-byte malformed input drives a
~21 GiB `malloc` and trips libfuzzer's OOM guard. Two distinct repros
per target are checked in at `fuzz/seeds/heif_atoms/regression-iloc-oom*`
and `fuzz/seeds/heif_xmp_probe/regression-iloc-oom*`. Run them through
the harness to reproduce:

```sh
cargo +nightly fuzz run heif_atoms fuzz/seeds/heif_atoms/regression-iloc-oom
```

The bug is upstream in `mp4-atom`'s `parse_vorbis_comment`, not in
kei's code. Filed as kixelated/mp4-atom#154. kei's defense-in-depth is
PR #286, which walks top-level boxes by header and only invokes the
iinf / iloc decoders. With #286 merged, neither harness reproduces the
seeds anymore; without it, `just fuzz run heif_atoms` trips the seed on
its first iteration.

To fuzz past the OOM (so libfuzzer can find *other* bugs in the same
target before #286 lands) raise the limit:

```sh
cargo +nightly fuzz run heif_atoms -- -max_total_time=60 -rss_limit_mb=4096
```

## How the harnesses reach kei internals

Every harness depends on the kei crate through the
`__fuzz_internals` Cargo feature, which gates a `kei::__fuzz` module
of `pub fn` wrappers around the parser entry points. The wrappers take
and return only externally-nameable types (bytes, strings,
`serde_json::Value`) and discard typed results internally, so kei's
internal types stay `pub(crate)`. Production builds without the
feature don't see the module at all.

Before the lib+bin split, harnesses inlined source files via
`#[path]`. That worked for leaf modules but broke for any parser
whose module imported `crate::xxx` or `super::xxx`. The four
non-leaf targets (`toml_config`, `auth_responses`,
`photo_asset_from_record`, `state_enums_from_str`) only became
fuzzable once the split landed.

## Adding a target

1. Pick something whose input crosses a trust boundary - network response, on-disk file, user config. A pure-internal helper isn't worth a fuzz target.
2. Add a `pub fn` wrapper in `src/lib.rs` under `pub mod __fuzz` that takes externally-nameable input (`&[u8]`, `&str`, `serde_json::Value`), calls the parser, and discards the typed result.
3. Write `fuzz/fuzz_targets/<name>.rs` that calls `kei::__fuzz::<your_wrapper>` from inside `fuzz_target!`.
4. Add a `[[bin]]` entry to `fuzz/Cargo.toml`.
5. `just fuzz build` to confirm it links.

## Seeds and regressions

`fuzz/seeds/<target>/` is checked in. `just fuzz run` passes it as a
read-only auxiliary corpus alongside the writable
`fuzz/corpus/<target>/`, so anything committed there replays on every
run. Use it for:

- repros for known crashes (filename prefix `regression-`)
- hand-crafted inputs that exercise a code path the fuzzer keeps missing

`corpus/<target>/` and `artifacts/<target>/` stay gitignored because
they grow into the megabytes and aren't deterministic across runs.
