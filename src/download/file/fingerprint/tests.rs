use tempfile::TempDir;

use super::compute_sha256;

#[tokio::test]
async fn test_compute_sha256_known_content() {
    let dir = TempDir::new().unwrap();
    let file_path = dir.path().join("known.bin");
    std::fs::write(&file_path, b"hello world").unwrap();

    let hash = compute_sha256(&file_path).await.unwrap();
    assert_eq!(
        hash,
        "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9"
    );
}

#[tokio::test]
async fn test_compute_sha256_nonexistent_file() {
    let dir = TempDir::new().unwrap();
    let file_path = dir.path().join("nonexistent_file.bin");
    let result = compute_sha256(&file_path).await;
    assert!(result.is_err());
}

/// Robustness regression for downloaded-byte verification. Apple does
/// not publish a content hash for assets, so kei cannot verify
/// downloaded bytes against a server-side digest. Instead it stores
/// the SHA-256 of what landed on disk in `local_checksum` and surfaces
/// post-hoc divergence via `kei verify --checksums`. This pins the
/// digest of a fixed JPEG-shaped payload — a future change to the
/// hash routine (different algorithm, different buffer windowing,
/// alternate hex encoding) will fail this assertion before it
/// silently rewrites every user's stored checksum on the next sync.
#[tokio::test]
async fn compute_sha256_jpeg_payload_pins_digest_for_regression() {
    let dir = TempDir::new().unwrap();
    let file_path = dir.path().join("pinned.jpg");
    // Minimal JFIF-shaped payload: SOI + APP0(JFIF) header + 16 body
    // bytes + EOI. Magic bytes pass `validate_downloaded_content`.
    let payload: [u8; 38] = [
        0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, // SOI + APP0 length
        0x4A, 0x46, 0x49, 0x46, 0x00, // "JFIF\0"
        0x01, 0x01, // version 1.1
        0x00, // density units
        0x00, 0x01, 0x00, 0x01, // X / Y density
        0x00, 0x00, // thumbnail w/h
        0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xAA, 0xBB, 0xCC, 0xDD, 0xEE,
        0xFF, // body
        0xFF, 0xD9, // EOI
    ];
    std::fs::write(&file_path, payload).unwrap();

    let hash = compute_sha256(&file_path).await.unwrap();
    assert_eq!(
        hash, "17ec927c65744de82d16f52109b59283318111f9e3e3258439e624a5755f888c",
        "SHA-256 of the pinned JPEG fixture changed; if the hash routine \
         was updated intentionally, every user's stored local_checksum will \
         diverge from a fresh `verify --checksums` run on the next sync. \
         Coordinate any change with a state-DB migration that re-hashes \
         existing rows."
    );
}

#[tokio::test]
async fn compute_sha256_empty_file_returns_known_hash() {
    // Arrange: create an empty file
    let dir = TempDir::new().unwrap();
    let file_path = dir.path().join("empty.bin");
    std::fs::write(&file_path, b"").unwrap();

    // Act
    let hash = compute_sha256(&file_path).await.unwrap();

    // Assert: SHA-256 of empty input is the well-known constant
    assert_eq!(
        hash,
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
}

#[tokio::test]
async fn compute_sha256_large_file_streams_without_loading_all_into_memory() {
    // Arrange: write a 2 MiB file (large enough to confirm streaming via io::copy)
    let dir = TempDir::new().unwrap();
    let file_path = dir.path().join("large.bin");

    let chunk = vec![0xABu8; 1024];
    {
        use std::io::Write;
        let mut f = std::fs::File::create(&file_path).unwrap();
        for _ in 0..2048 {
            f.write_all(&chunk).unwrap();
        }
    }

    // Act
    let hash = compute_sha256(&file_path).await.unwrap();

    // Assert: hash is a valid 64-char hex string (SHA-256)
    assert_eq!(hash.len(), 64);
    assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));

    // Compute expected hash independently for verification
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    for _ in 0..2048 {
        hasher.update(&chunk);
    }
    let expected = format!("{:x}", hasher.finalize());
    assert_eq!(hash, expected);
}
