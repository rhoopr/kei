//! Sparse synthetic inputs exercise the real probe, prepare, validation and
//! publication graph under actual Rust allocation high-water measurement.

use super::heif_writer::prepare_metadata_heif;
use super::probe::{HeifNativeCaptureTime, probe_exif};
use super::test_support::embed_temp_entries;
use super::values::MetadataWrite;
use super::xmp_fields::ensure_initialized;
use crate::download::file::ExistingFileFingerprint;
use crate::download::heif;
use crate::test_helpers::allocation::{HeapHighWater, measure_heap_high_water};
use sha2::{Digest, Sha256};
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

const BODY_MARKER: &[u8] = b"kei sparse synthetic media";
// Longer than the copy buffer, so it crosses a boundary regardless of the
// preceding metadata patch offsets.
const BOUNDARY_MARKER: &[u8] = &[b'z'; 64 * 1024 + 37];
const TAIL_MARKER: &[u8] = b"unchanged final media bytes";
const BOUNDARY_OFFSET: u64 = 64 * 1024 - 9;

fn capture_tiff() -> Vec<u8> {
    // IFD0 points at the two-entry Exif IFD. ASCII storage is deliberately
    // separate, so a native repair can change only its intended fields.
    let mut bytes = b"II\x2a\0\x08\0\0\0".to_vec();
    let entry = |tag: u16, kind: u16, count: u32, value: u32| {
        [
            tag.to_le_bytes().as_slice(),
            kind.to_le_bytes().as_slice(),
            count.to_le_bytes().as_slice(),
            value.to_le_bytes().as_slice(),
        ]
        .concat()
    };
    bytes.extend_from_slice(&1_u16.to_le_bytes());
    bytes.extend_from_slice(&entry(0x8769, 4, 1, 26));
    bytes.extend_from_slice(&0_u32.to_le_bytes());
    bytes.extend_from_slice(&2_u16.to_le_bytes());
    bytes.extend_from_slice(&entry(0x9003, 2, 20, 56));
    bytes.extend_from_slice(&entry(0x9011, 2, 7, 76));
    bytes.extend_from_slice(&0_u32.to_le_bytes());
    bytes.extend_from_slice(b"2023:09:03 09:28:14\0+03:00\0");
    bytes
}

fn sparse_heif(path: &Path, size: u64) -> u64 {
    let mut prefix = heif::apple_multi_exif_heic(&capture_tiff());
    let body_start = prefix.len() as u64 + 16;
    // The primary grid's first hvc1 tile now owns the entire sparse body, so
    // production post-rewrite validation must compare every large media byte.
    heif::set_test_file_item_extent(
        &mut prefix,
        2,
        u32::try_from(body_start).unwrap(),
        u32::try_from(size - body_start).unwrap(),
    );
    let mut file = File::create(path).unwrap();
    file.write_all(&prefix).unwrap();
    let mdat_len = size - prefix.len() as u64;
    file.write_all(&1_u32.to_be_bytes()).unwrap();
    file.write_all(b"mdat").unwrap();
    file.write_all(&mdat_len.to_be_bytes()).unwrap();
    file.write_all(BODY_MARKER).unwrap();
    file.seek(SeekFrom::Start(body_start + BOUNDARY_OFFSET))
        .unwrap();
    file.write_all(BOUNDARY_MARKER).unwrap();
    file.set_len(size).unwrap();
    file.seek(SeekFrom::End(-(TAIL_MARKER.len() as i64)))
        .unwrap();
    file.write_all(TAIL_MARKER).unwrap();
    size - body_start
}

fn fingerprint(path: &Path) -> ExistingFileFingerprint {
    let mut file = File::open(path).unwrap();
    let size = file.metadata().unwrap().len();
    let mut hash = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = file.read(&mut buffer).unwrap();
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
    }
    ExistingFileFingerprint {
        size,
        sha256: hash.finalize().into(),
    }
}

fn assert_sparse_media_preserved(path: &Path, expected_body_len: u64) {
    let mut file = File::open(path).unwrap();
    let end = file.metadata().unwrap().len();
    let mut start = 0_u64;
    while start < end {
        file.seek(SeekFrom::Start(start)).unwrap();
        let mut head = [0_u8; 16];
        file.read_exact(&mut head[..8]).unwrap();
        let size32 = u32::from_be_bytes(head[..4].try_into().unwrap());
        let (size, header_len) = match size32 {
            0 => (end - start, 8),
            1 => {
                file.read_exact(&mut head[8..]).unwrap();
                (u64::from_be_bytes(head[8..].try_into().unwrap()), 16)
            }
            size => (u64::from(size), 8),
        };
        assert!(size >= header_len && start + size <= end);
        if &head[4..8] == b"mdat" && size - header_len == expected_body_len {
            let body = start + header_len;
            for (offset, expected) in [
                (0, BODY_MARKER),
                (BOUNDARY_OFFSET, BOUNDARY_MARKER),
                (expected_body_len - TAIL_MARKER.len() as u64, TAIL_MARKER),
            ] {
                file.seek(SeekFrom::Start(body + offset)).unwrap();
                let mut actual = vec![0_u8; expected.len()];
                file.read_exact(&mut actual).unwrap();
                assert_eq!(actual, expected, "unchanged media at {offset}");
            }
            file.seek(SeekFrom::Start(
                body + BOUNDARY_OFFSET + BOUNDARY_MARKER.len() as u64 + 4096,
            ))
            .unwrap();
            let mut hole = [1_u8; 32];
            file.read_exact(&mut hole).unwrap();
            assert_eq!(hole, [0_u8; 32]);
            return;
        }
        start += size;
    }
    panic!("rewritten container lost its large synthetic mdat");
}

fn assert_heap_bound(measured: &HeapHighWater, stage: &str) {
    assert!(
        measured.peak_growth < 4 * 1024 * 1024,
        "{stage} retained whole-file bytes: {measured:?}"
    );
    assert!(
        measured.largest_allocation < 1024 * 1024,
        "{stage} allocated a whole-file buffer: {measured:?}"
    );
}

#[test]
fn heif_sparse_metadata_heap_is_independent_of_media_size() {
    ensure_initialized();
    let dir = tempfile::tempdir().unwrap();
    let mut peaks = Vec::new();
    for size in [1024 * 1024, 128 * 1024 * 1024] {
        let path = dir.path().join(format!("sparse-{size}.heic"));
        let media_len = sparse_heif(&path, size);
        let expected_input = fingerprint(&path);
        let (probe, measured_probe) = measure_heap_high_water(|| probe_exif(&path).unwrap());
        assert_eq!(
            probe.datetime_original.as_deref(),
            Some("2023:09:03 09:28:14")
        );
        assert_heap_bound(&measured_probe, "probe");

        let write = MetadataWrite {
            datetime: Some("2024:06:15 10:00:00".into()),
            offset_time_original: Some("+11:00".into()),
            require_native_heif_capture_time: true,
            rating: Some(5),
            ..MetadataWrite::default()
        };
        let (expected_output, measured_prepare) = measure_heap_high_water(|| {
            let prepared =
                prepare_metadata_heif(&path, &write, ".meta-tmp", Some(expected_input)).unwrap();
            let output = prepared.output_fingerprint();
            assert_eq!(prepared.publish(&path).unwrap(), output);
            output
        });
        assert_heap_bound(
            &measured_prepare,
            "native repair, XMP insertion, validation and publication",
        );
        assert_eq!(fingerprint(&path), expected_output);
        assert_sparse_media_preserved(&path, media_len);
        let repaired = probe_exif(&path).unwrap();
        assert!(matches!(repaired.native_heif_capture_time,
            HeifNativeCaptureTime::Present { ref datetime_original, ref offset_time_original, .. }
                if datetime_original.as_deref() == Some("2024:06:15 10:00:00")
                    && offset_time_original.as_deref() == Some("+11:00")));

        let replacement = MetadataWrite {
            rating: Some(2),
            ..MetadataWrite::default()
        };
        let (_, measured_replace) = measure_heap_high_water(|| {
            prepare_metadata_heif(&path, &replacement, ".meta-tmp", Some(expected_output))
                .unwrap()
                .publish(&path)
                .unwrap();
        });
        assert_heap_bound(&measured_replace, "existing XMP replacement and validation");
        assert_sparse_media_preserved(&path, media_len);
        assert!(embed_temp_entries(dir.path(), ".meta-tmp").is_empty());
        peaks.push((
            measured_probe.peak_growth,
            measured_prepare.peak_growth,
            measured_replace.peak_growth,
        ));
    }
    for (small, large) in [
        (peaks[0].0, peaks[1].0),
        (peaks[0].1, peaks[1].1),
        (peaks[0].2, peaks[1].2),
    ] {
        assert!(
            large <= small + 64 * 1024,
            "heap high-water grew with unchanged media: 1 MiB={small}, 128 MiB={large}"
        );
    }
    eprintln!(
        "HEIF real Rust heap peaks (probe, native prepare/publish, XMP replace/publish): {peaks:?}"
    );
}

#[test]
fn heif_sparse_stale_input_preserves_media_and_cleans_temporary_files() {
    ensure_initialized();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("stale.heic");
    sparse_heif(&path, 1024 * 1024);
    let approved = fingerprint(&path);
    let mut source = fs::OpenOptions::new().write(true).open(&path).unwrap();
    source.seek(SeekFrom::End(-1)).unwrap();
    source.write_all(b"!").unwrap();
    drop(source);
    let external = fingerprint(&path);
    let error = prepare_metadata_heif(
        &path,
        &MetadataWrite {
            rating: Some(5),
            ..MetadataWrite::default()
        },
        ".meta-tmp",
        Some(approved),
    )
    .err()
    .expect("stale planned source must refuse");
    assert!(crate::download::file::classify_conditional_publish_error(&error).target_changed);
    assert_eq!(fingerprint(&path), external);
    assert!(embed_temp_entries(dir.path(), ".meta-tmp").is_empty());
}

// This public libavif fixture retains its original encoded AV1 payload and
// properties. Only container placement is changed; no codec bytes are invented.
const PUBLIC_AVIF: &[u8] = include_bytes!("../../../tests/data/white_1x1.avif");

fn fixture_atom(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut bytes = u32::try_from(body.len() + 8)
        .unwrap()
        .to_be_bytes()
        .to_vec();
    bytes.extend_from_slice(kind);
    bytes.extend_from_slice(body);
    bytes
}

fn public_avif_codec() -> &'static [u8] {
    assert_eq!(&PUBLIC_AVIF[274..282], b"\0\0\0\x1fmdat");
    &PUBLIC_AVIF[282..305]
}

fn write_sparse_reserve(file: &mut File, length: u64) {
    let start = file.stream_position().unwrap();
    file.write_all(BODY_MARKER).unwrap();
    file.seek(SeekFrom::Start(start + BOUNDARY_OFFSET)).unwrap();
    file.write_all(BOUNDARY_MARKER).unwrap();
    file.seek(SeekFrom::Start(start + length - TAIL_MARKER.len() as u64))
        .unwrap();
    file.write_all(TAIL_MARKER).unwrap();
    assert_eq!(file.stream_position().unwrap(), start + length);
}

/// Independently encode iloc v1 / construction method 1, relocating the
/// original AV1 item into idat. Large unused idat bytes or a sibling free box
/// are sparse reserves; the coded item is exactly the public fixture's 23 bytes.
fn valid_avif_with_large_meta(path: &Path, reserve: u64, opaque: bool) {
    let codec = public_avif_codec();
    let mut iloc = vec![1, 0, 0, 0, 0x44, 0];
    for value in [1_u16, 1, 1, 0, 1] {
        // Item count, item ID, construction method, data reference, extent count.
        iloc.extend_from_slice(&value.to_be_bytes());
    }
    iloc.extend_from_slice(
        &u32::try_from(if opaque { 0 } else { reserve })
            .unwrap()
            .to_be_bytes(),
    );
    iloc.extend_from_slice(&u32::try_from(codec.len()).unwrap().to_be_bytes());
    let iloc = fixture_atom(b"iloc", &iloc);
    let mut controls = Vec::new();
    let mut cursor = 44_usize; // Original FullBox meta children begin here.
    while cursor < 274 {
        let size = u32::from_be_bytes(PUBLIC_AVIF[cursor..cursor + 4].try_into().unwrap()) as usize;
        if &PUBLIC_AVIF[cursor + 4..cursor + 8] == b"iloc" {
            controls.extend_from_slice(&iloc);
        } else {
            controls.extend_from_slice(&PUBLIC_AVIF[cursor..cursor + size]);
        }
        cursor += size;
    }
    assert_eq!(cursor, 274);
    let idat_size = 8 + codec.len() as u64 + if opaque { 0 } else { reserve };
    let free_size = if opaque { 8 + reserve } else { 0 };
    let meta_size = 12 + controls.len() as u64 + idat_size + free_size;
    let mut file = File::create(path).unwrap();
    file.write_all(&PUBLIC_AVIF[..32]).unwrap();
    file.write_all(&u32::try_from(meta_size).unwrap().to_be_bytes())
        .unwrap();
    file.write_all(b"meta\0\0\0\0").unwrap();
    file.write_all(&controls).unwrap();
    file.write_all(&u32::try_from(idat_size).unwrap().to_be_bytes())
        .unwrap();
    file.write_all(b"idat").unwrap();
    if !opaque {
        write_sparse_reserve(&mut file, reserve);
    }
    file.write_all(codec).unwrap();
    if opaque {
        file.write_all(&u32::try_from(free_size).unwrap().to_be_bytes())
            .unwrap();
        file.write_all(b"free").unwrap();
        write_sparse_reserve(&mut file, reserve);
    }
    assert_eq!(file.stream_position().unwrap(), 32 + meta_size);
}

// Independent test-side box walking uses file seeks and fixed-size headers,
// rather than the production FileLayout or a whole-container buffer.
fn fixture_box(
    file: &mut File,
    mut cursor: u64,
    end: u64,
    wanted: &[u8; 4],
) -> Option<std::ops::Range<u64>> {
    while cursor < end {
        file.seek(SeekFrom::Start(cursor)).unwrap();
        let mut header = [0_u8; 16];
        file.read_exact(&mut header[..8]).unwrap();
        let size32 = u32::from_be_bytes(header[..4].try_into().unwrap());
        let (size, head) = match size32 {
            0 => (end - cursor, 8),
            1 => {
                file.read_exact(&mut header[8..]).unwrap();
                (u64::from_be_bytes(header[8..].try_into().unwrap()), 16)
            }
            size => (u64::from(size), 8),
        };
        assert!(size >= head && cursor + size <= end);
        if &header[4..8] == wanted {
            return Some(cursor + head..cursor + size);
        }
        cursor += size;
    }
    assert_eq!(cursor, end);
    None
}

fn meta_child(path: &Path, kind: &[u8; 4]) -> Option<std::ops::Range<u64>> {
    let mut file = File::open(path).unwrap();
    let end = file.metadata().unwrap().len();
    let meta = fixture_box(&mut file, 0, end, b"meta")?;
    fixture_box(&mut file, meta.start + 4, meta.end, kind)
}

fn range_hash(path: &Path, range: std::ops::Range<u64>) -> [u8; 32] {
    let mut file = File::open(path).unwrap();
    file.seek(SeekFrom::Start(range.start)).unwrap();
    let mut remaining = range.end - range.start;
    let mut buffer = [0_u8; 64 * 1024];
    let mut hash = Sha256::new();
    while remaining > 0 {
        let count = usize::try_from(remaining.min(buffer.len() as u64)).unwrap();
        file.read_exact(&mut buffer[..count]).unwrap();
        hash.update(&buffer[..count]);
        remaining -= count as u64;
    }
    hash.finalize().into()
}

fn assert_public_avif_idat_codec(path: &Path) {
    let idat = meta_child(path, b"idat").expect("retained idat");
    let iloc = meta_child(path, b"iloc").unwrap();
    assert!(iloc.end - iloc.start < 4096);
    let mut file = File::open(path).unwrap();
    file.seek(SeekFrom::Start(iloc.start)).unwrap();
    let mut bytes = vec![0; usize::try_from(iloc.end - iloc.start).unwrap()];
    file.read_exact(&mut bytes).unwrap();
    let version = bytes[0];
    assert!((1..=2).contains(&version));
    let offset_width = bytes[4] >> 4;
    let length_width = bytes[4] & 15;
    let base_width = bytes[5] >> 4;
    let index_width = bytes[5] & 15;
    let mut cursor = 6;
    let integer = |cursor: &mut usize, width: u8| {
        let end = *cursor + usize::from(width);
        let value = bytes[*cursor..end]
            .iter()
            .fold(0_u64, |value, byte| (value << 8) | u64::from(*byte));
        *cursor = end;
        value
    };
    let count = integer(&mut cursor, if version == 2 { 4 } else { 2 });
    let mut primary = None;
    for _ in 0..count {
        let id = integer(&mut cursor, if version == 2 { 4 } else { 2 });
        let construction = integer(&mut cursor, 2) & 15;
        let reference = integer(&mut cursor, 2);
        let base = integer(&mut cursor, base_width);
        let extents = integer(&mut cursor, 2);
        for _ in 0..extents {
            let _index = integer(&mut cursor, index_width);
            let offset = integer(&mut cursor, offset_width);
            let length = integer(&mut cursor, length_width);
            if id == 1 {
                assert_eq!((construction, reference, extents), (1, 0, 1));
                primary = Some(idat.start + base + offset..idat.start + base + offset + length);
            }
        }
    }
    assert_eq!(cursor, bytes.len());
    let codec = primary.expect("public AV1 primary item");
    assert!(codec.start >= idat.start && codec.end <= idat.end);
    assert_eq!(codec.end - codec.start, public_avif_codec().len() as u64);
    file.seek(SeekFrom::Start(codec.start)).unwrap();
    let mut actual = vec![0_u8; public_avif_codec().len()];
    file.read_exact(&mut actual).unwrap();
    assert_eq!(actual, public_avif_codec(), "original AV1 codec payload");
}

fn inspect_valid_avif(path: &Path, label: &str) {
    if let Some(root) = std::env::var_os("KEI_HEIF_STREAM_INSPECT_DIR") {
        let root = std::path::PathBuf::from(root);
        fs::create_dir_all(&root).unwrap();
        fs::copy(path, root.join(format!("{label}.avif"))).unwrap();
    }
}

#[test]
fn heif_valid_avif_large_idat_and_opaque_meta_preserve_codec_with_bounded_heap() {
    ensure_initialized();
    let dir = tempfile::tempdir().unwrap();
    for (opaque, label) in [(false, "large-idat"), (true, "large-opaque")] {
        let mut peaks = Vec::new();
        // Start above the former meta-box ceiling, so the old implementation
        // fails by refusing a valid container before any allocation assertion.
        for reserve in [16 * 1024 * 1024, 1024 * 1024] {
            let path = dir.path().join(format!("{label}-{reserve}.avif"));
            valid_avif_with_large_meta(&path, reserve, opaque);
            assert_public_avif_idat_codec(&path);
            let protected = if opaque { b"free" } else { b"idat" };
            let original_range = meta_child(&path, protected).unwrap();
            assert!(original_range.end - original_range.start >= reserve);
            let unchanged = range_hash(&path, original_range);
            let properties = range_hash(&path, meta_child(&path, b"iprp").unwrap());
            let (probe, probe_heap) = measure_heap_high_water(|| probe_exif(&path).unwrap());
            // The separate Exif/GPS locator must skip large idat and opaque
            // payloads instead of rejecting the complete meta-box size.
            let mut source = File::open(&path).unwrap();
            let source_len = source.metadata().unwrap().len();
            assert_eq!(
                heif::locate_exif_tiff(&mut source, source_len).unwrap(),
                None
            );
            assert!(probe.datetime_original.is_none());
            assert_heap_bound(&probe_heap, "valid AVIF probe with large meta child");
            if reserve > 8 * 1024 * 1024 {
                inspect_valid_avif(&path, &format!("{label}-before"));
            }
            let mut stages = vec![probe_heap.peak_growth];
            for (rating, stage) in [(5, "insert"), (2, "replace")] {
                let approved = fingerprint(&path);
                let write = MetadataWrite {
                    rating: Some(rating),
                    ..MetadataWrite::default()
                };
                let (output, heap) = measure_heap_high_water(|| {
                    prepare_metadata_heif(&path, &write, ".meta-tmp", Some(approved))
                        .unwrap()
                        .publish(&path)
                        .unwrap()
                });
                assert_heap_bound(
                    &heap,
                    "valid AVIF rewrite, validation and publication with large meta child",
                );
                assert_eq!(fingerprint(&path), output);
                assert_eq!(
                    range_hash(&path, meta_child(&path, protected).unwrap()),
                    unchanged
                );
                assert_eq!(
                    range_hash(&path, meta_child(&path, b"iprp").unwrap()),
                    properties,
                    "original AV1 image properties"
                );
                assert_public_avif_idat_codec(&path);
                assert!(embed_temp_entries(dir.path(), ".meta-tmp").is_empty());
                if reserve > 8 * 1024 * 1024 {
                    inspect_valid_avif(&path, &format!("{label}-after-{stage}"));
                }
                stages.push(heap.peak_growth);
            }
            peaks.push(stages);
        }
        for (large, small) in peaks[0].iter().zip(&peaks[1]) {
            assert!(
                *large <= small + 64 * 1024,
                "{label} heap scaled with untouched meta payload: {peaks:?}"
            );
        }
        eprintln!(
            "Public AVIF {label} real Rust heap peaks (16 MiB reserve,1 MiB reserve): {peaks:?}"
        );
    }
}

#[test]
fn heif_valid_avif_many_top_level_free_boxes_remain_supported_with_bounded_heap() {
    ensure_initialized();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("many-boxes.avif");
    let mut source = File::create(&path).unwrap();
    source.write_all(PUBLIC_AVIF).unwrap();
    for _ in 0..65_537 {
        source.write_all(b"\0\0\0\x08free").unwrap();
    }
    drop(source);
    let (probe, probe_heap) = measure_heap_high_water(|| probe_exif(&path).unwrap());
    assert!(probe.datetime_original.is_none());
    assert_heap_bound(&probe_heap, "many top-level boxes probe");
    let approved = fingerprint(&path);
    let write = MetadataWrite {
        rating: Some(4),
        ..MetadataWrite::default()
    };
    let (output, heap) = measure_heap_high_water(|| {
        prepare_metadata_heif(&path, &write, ".meta-tmp", Some(approved))
            .unwrap()
            .publish(&path)
            .unwrap()
    });
    assert_heap_bound(&heap, "many top-level boxes rewrite and validation");
    assert_eq!(fingerprint(&path), output);
    let mut file = File::open(&path).unwrap();
    let end = file.metadata().unwrap().len();
    let mut cursor = 0;
    let mut count = 0;
    while cursor < end {
        file.seek(SeekFrom::Start(cursor)).unwrap();
        let mut header = [0; 8];
        file.read_exact(&mut header).unwrap();
        let size = u32::from_be_bytes(header[..4].try_into().unwrap());
        assert!(size >= 8);
        if &header[4..] == b"free" {
            assert_eq!(size, 8);
            count += 1;
        }
        cursor += u64::from(size);
    }
    assert_eq!((cursor, count), (end, 65_537));
    assert!(embed_temp_entries(dir.path(), ".meta-tmp").is_empty());
}

#[test]
fn heif_valid_avif_large_meta_validation_rejects_late_file_corruption() {
    ensure_initialized();
    let dir = tempfile::tempdir().unwrap();
    for opaque in [false, true] {
        let path = dir.path().join(format!("late-{opaque}.avif"));
        valid_avif_with_large_meta(&path, 16 * 1024 * 1024, opaque);
        let approved = fingerprint(&path);
        let prepared = prepare_metadata_heif(
            &path,
            &MetadataWrite {
                rating: Some(3),
                ..MetadataWrite::default()
            },
            ".meta-tmp",
            Some(approved),
        )
        .unwrap();
        let error = prepared
            .publish_with(&path, |temporary, _destination, _input, _output| {
                let mut source = File::open(&path).unwrap();
                let before = heif::FileLayout::read(&mut source, approved.size).unwrap();
                let mut output = fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(temporary)
                    .unwrap();
                let output_len = output.metadata().unwrap().len();
                let after = heif::FileLayout::read(&mut output, output_len).unwrap();
                let protected = if opaque { b"free" } else { b"idat" };
                let range = meta_child(temporary, protected).unwrap();
                assert!(range.end - range.start > 8 * 1024 * 1024);
                // The layouts have already been read. Corrupt only the final byte,
                // beyond 256 compare buffers, so stale in-memory metadata cannot
                // supply the validation evidence for the opaque/idat payload.
                output.seek(SeekFrom::Start(range.end - 1)).unwrap();
                let mut byte = [0_u8];
                output.read_exact(&mut byte).unwrap();
                output.seek(SeekFrom::Start(range.end - 1)).unwrap();
                output.write_all(&[byte[0] ^ 1]).unwrap();
                let (result, heap) = measure_heap_high_water(|| {
                    before.validate(&mut source, &after, &mut output, None)
                });
                assert_heap_bound(&heap, "late opaque/idat corruption validation");
                let failure = result.expect_err(
                    "production validator must read and reject the changed late file byte",
                );
                assert!(failure.to_string().contains("changed"), "{failure}");
                Err(failure.into())
            })
            .unwrap_err();
        assert!(format!("{error:#}").contains("changed"));
        assert_eq!(fingerprint(&path), approved);
        assert_public_avif_idat_codec(&path);
        assert!(embed_temp_entries(dir.path(), ".meta-tmp").is_empty());
    }
}

#[test]
fn heif_native_exif_near_retained_allowance_repairs_with_bounded_operational_heap() {
    ensure_initialized();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("large-native-exif.heic");
    const TIFF_BYTES: usize = 7 * 1024 * 1024;
    const RETAINED_UNIT: usize = 8 * 1024 * 1024;
    // A fixed operational regression ceiling allows eight retained units.
    // This is distinct from the tiny-control/media-size independence tests;
    // actual measured peaks are printed, rather than assumed from this limit.
    const OPERATIONAL_HEAP_CEILING: usize = 8 * RETAINED_UNIT;
    let opaque_start;
    {
        let mut tiff = capture_tiff();
        opaque_start = tiff.len();
        tiff.resize(TIFF_BYTES, 0);
        tiff[opaque_start..opaque_start + BODY_MARKER.len()].copy_from_slice(BODY_MARKER);
        tiff[opaque_start + BOUNDARY_OFFSET as usize
            ..opaque_start + BOUNDARY_OFFSET as usize + BOUNDARY_MARKER.len()]
            .copy_from_slice(BOUNDARY_MARKER);
        tiff[TIFF_BYTES - TAIL_MARKER.len()..].copy_from_slice(TAIL_MARKER);
        // Synthetic item graph, with a valid native TIFF and opaque trailing
        // TIFF storage. The synthetic image items are not codec-valid claims.
        fs::write(&path, heif::apple_multi_exif_heic(&tiff)).unwrap();
    }
    let opaque_hash = |path: &Path| {
        let mut file = File::open(path).unwrap();
        let len = file.metadata().unwrap().len();
        let (start, length) = heif::locate_exif_tiff(&mut file, len).unwrap().unwrap();
        assert_eq!(length, TIFF_BYTES as u64);
        range_hash(path, start + opaque_start as u64..start + length)
    };
    let original_opaque = opaque_hash(&path);
    let approved = fingerprint(&path);
    let (probe, probe_heap) = measure_heap_high_water(|| probe_exif(&path).unwrap());
    assert_eq!(
        probe.datetime_original.as_deref(),
        Some("2023:09:03 09:28:14")
    );
    let write = MetadataWrite {
        datetime: Some("2024:06:15 10:00:00".into()),
        offset_time_original: Some("+11:00".into()),
        require_native_heif_capture_time: true,
        rating: Some(5),
        ..MetadataWrite::default()
    };
    let (output, repair_heap) = measure_heap_high_water(|| {
        prepare_metadata_heif(&path, &write, ".meta-tmp", Some(approved))
            .unwrap()
            .publish(&path)
            .unwrap()
    });
    for heap in [&probe_heap, &repair_heap] {
        assert!(
            heap.peak_growth < OPERATIONAL_HEAP_CEILING,
            "near-limit native Exif heap: {heap:?}"
        );
        assert!(
            (TIFF_BYTES..=RETAINED_UNIT).contains(&heap.largest_allocation),
            "must exercise a real retained TIFF allocation within the declared allowance: {heap:?}"
        );
    }
    assert_eq!(fingerprint(&path), output);
    assert_eq!(opaque_hash(&path), original_opaque);
    let repaired = probe_exif(&path).unwrap();
    assert!(matches!(repaired.native_heif_capture_time,
        HeifNativeCaptureTime::Present { ref datetime_original, ref offset_time_original, .. }
            if datetime_original.as_deref() == Some("2024:06:15 10:00:00")
                && offset_time_original.as_deref() == Some("+11:00")));
    assert!(embed_temp_entries(dir.path(), ".meta-tmp").is_empty());
    eprintln!(
        "7 MiB native TIFF actual Rust heap: probe={probe_heap:?}, repair+validate+publish={repair_heap:?}"
    );
}

#[cfg(feature = "__fuzz_internals")]
#[test]
fn heif_valid_avif_large_meta_fuzz_oracle_agrees_with_production() {
    let directory = tempfile::tempdir().unwrap();
    for opaque in [false, true] {
        let path = directory.path().join(format!("fuzz-opaque-{opaque}.avif"));
        valid_avif_with_large_meta(&path, 9 * 1024 * 1024, opaque);
        // Whole byte slices are deliberately confined to this independent
        // legacy oracle; production receives file handles and streams ranges.
        let input = fs::read(&path).unwrap();
        heif::fuzz_rewrite_xmp_preserves(&input);
    }
}
