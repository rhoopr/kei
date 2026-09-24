//! Shared metadata test fixtures and invocation helpers.

#[cfg(feature = "xmp")]
use super::embedded::apply_metadata;
#[cfg(feature = "xmp")]
use super::sidecar::write_sidecar;
#[cfg(feature = "xmp")]
use super::values::{MetadataWrite, XmpRational};
#[cfg(all(test, feature = "xmp"))]
use super::xmp_fields::build_xmp_packet;
#[cfg(feature = "xmp")]
use super::xmp_fields::ensure_initialized;
#[cfg(feature = "xmp")]
use crate::download::heif;
#[cfg(feature = "xmp")]
use anyhow::Result;
use std::fs;
use std::path::Path;
#[cfg(feature = "xmp")]
use std::path::PathBuf;
#[cfg(feature = "xmp")]
use xmp_toolkit::{OpenFileOptions, XmpFile, XmpMeta};

#[cfg(all(test, feature = "xmp"))]
pub(super) fn temp_path_for(path: &Path, temp_suffix: &str) -> PathBuf {
    let mut tmp_name = path.file_name().unwrap_or_default().to_os_string();
    tmp_name.push(temp_suffix);
    path.with_file_name(tmp_name)
}

#[cfg(feature = "xmp")]
pub(super) fn apply_metadata_with_default_suffix(
    path: &std::path::Path,
    write: &MetadataWrite,
) -> Result<()> {
    apply_metadata(path, write, ".meta-tmp")
}

#[cfg(feature = "xmp")]
pub(super) fn write_sidecar_with_default_suffix(
    path: &std::path::Path,
    write: &MetadataWrite,
) -> Result<()> {
    static RUNTIME: std::sync::OnceLock<tokio::runtime::Runtime> = std::sync::OnceLock::new();
    RUNTIME
        .get_or_init(|| tokio::runtime::Runtime::new().expect("sidecar test runtime"))
        .block_on(write_sidecar(path, write, ".meta-tmp"))
}

#[cfg(feature = "xmp")]
pub(super) fn xmp_rational(numerator: u32, denominator: u32) -> XmpRational {
    XmpRational {
        numerator,
        denominator,
    }
}

#[cfg(feature = "xmp")]
pub(super) fn test_tmp_dir(subdir: &str) -> PathBuf {
    std::env::temp_dir().join("claude").join(subdir)
}

#[cfg(feature = "xmp")]
/// Minimal valid JPEG (SOI + APP0 JFIF + EOI).
pub(super) fn minimal_jpeg() -> Vec<u8> {
    vec![
        0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46, 0x49, 0x46, 0x00, 0x01, 0x01, 0x00, 0x00,
        0x01, 0x00, 0x01, 0x00, 0x00, 0xFF, 0xD9,
    ]
}

#[cfg(feature = "xmp")]
pub(super) fn fresh_jpeg(dir: &Path, name: &str) -> PathBuf {
    fs::create_dir_all(dir).unwrap();
    let path = dir.join(name);
    fs::write(&path, minimal_jpeg()).unwrap();
    path
}

#[cfg(feature = "xmp")]
pub(super) fn read_meta(path: &Path) -> XmpMeta {
    ensure_initialized();
    let mut file = XmpFile::new().unwrap();
    file.open_file(path, OpenFileOptions::default().for_read())
        .unwrap();
    file.xmp().expect("no XMP in file")
}

#[cfg(feature = "xmp")]
pub(super) const SAMPLE_HEIC: &[u8] = include_bytes!("../../../tests/data/sample.heic");

#[cfg(feature = "xmp")]
pub(super) const SAMPLE_AVIF: &[u8] = include_bytes!("../../../tests/data/white_1x1.avif");

#[cfg(feature = "xmp")]
pub(super) fn fresh_heic(dir: &Path, name: &str) -> PathBuf {
    fs::create_dir_all(dir).unwrap();
    let path = dir.join(name);
    fs::write(&path, SAMPLE_HEIC).unwrap();
    path
}

#[cfg(feature = "xmp")]
pub(super) fn embed_temp_entries(dir: &Path, temp_suffix: &str) -> Vec<PathBuf> {
    let prefix = format!(".kei-metadata-{}-", std::process::id());
    fs::read_dir(dir)
        .unwrap()
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            (name.starts_with(&prefix) && name.ends_with(temp_suffix)).then(|| entry.path())
        })
        .collect()
}

#[cfg(feature = "xmp")]
pub(super) fn heic_with_xmp_packet(xmp: &[u8]) -> Vec<u8> {
    let mut bytes = Vec::new();
    heif::rewrite_xmp(SAMPLE_HEIC, xmp, &mut bytes)
        .expect("sample HEIC should accept a seed XMP packet");
    bytes
}

#[cfg(feature = "xmp")]
pub(super) fn write_seeded_heic(dir: &Path, name: &str, seed: &MetadataWrite) -> PathBuf {
    fs::create_dir_all(dir).unwrap();
    let path = dir.join(name);
    let seed_xmp = build_xmp_packet(seed).expect("seed XMP packet");
    fs::write(&path, heic_with_xmp_packet(&seed_xmp)).unwrap();
    path
}

#[cfg(feature = "xmp")]
pub(super) fn heif_ftyp_without_meta() -> Vec<u8> {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&24u32.to_be_bytes());
    bytes.extend_from_slice(b"ftyp");
    bytes.extend_from_slice(b"heic");
    bytes.extend_from_slice(&0u32.to_be_bytes());
    bytes.extend_from_slice(b"heic");
    bytes.extend_from_slice(b"mif1");
    bytes
}

#[cfg(not(feature = "xmp"))]
/// Minimal valid JPEG (SOI + APP0 JFIF + EOI).
pub(super) fn minimal_jpeg() -> Vec<u8> {
    vec![
        0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46, 0x49, 0x46, 0x00, 0x01, 0x01, 0x00, 0x00,
        0x01, 0x00, 0x01, 0x00, 0x00, 0xFF, 0xD9,
    ]
}

#[cfg(not(feature = "xmp"))]
pub(super) fn fresh_jpeg(dir: &Path, name: &str) -> std::path::PathBuf {
    let path = dir.join(name);
    fs::write(&path, minimal_jpeg()).unwrap();
    path
}
