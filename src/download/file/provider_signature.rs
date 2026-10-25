//! Explicit CloudKit MMCS v1 whole-file content comparison.
//!
//! OpenBubbles/rustpush prepare_cloudkit_put uses prefix 0x01 with SHA-1
//! over two NUL-terminated salts followed by every plaintext byte:
//! <https://github.com/OpenBubbles/rustpush/blob/b3c96c23e1aae78d1229ebaccb12027624f1eaff/src/icloud/mmcs.rs>
//! This is NOT raw SHA-1 or local SHA-256. Keyed prefix 0x04 and unknown
//! encodings are unsupported. Current provider identity is checked separately.
//! Interpret this only as the selected resource's fileChecksum: other MMCS
//! fields also use prefix 0x01 for different algorithms (including Ford keys).

use super::fingerprint::ExistingFileFingerprint;
use sha1::{Digest, Sha1};
use sha2::Sha256;
use std::io::Read;

pub(super) fn supported(checksum: &str) -> Option<[u8; 20]> {
    if checksum.len() != 28 {
        return None;
    }
    let decoded = data_encoding::BASE64.decode(checksum.as_bytes()).ok()?;
    let [0x01, digest @ ..] = decoded.as_slice() else {
        return None;
    };
    digest.try_into().ok()
}

/// Hash one retained file in one bounded-memory pass. The separate local
/// fingerprint lets the caller reject changes since initial path validation.
pub(super) fn compare(
    reader: &mut impl Read,
    expected: &[u8; 20],
) -> std::io::Result<(bool, ExistingFileFingerprint)> {
    let mut provider = Sha1::new();
    provider.update(b"com.apple.XattrObjectSalt\0com.apple.DataObjectSalt\0");
    let mut local = Sha256::new();
    let mut size = 0_u64;
    let mut buffer = [0_u8; 65_536];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        let bytes = buffer
            .get(..read)
            .ok_or_else(|| std::io::Error::other("reader returned an invalid byte count"))?;
        size = size
            .checked_add(read as u64)
            .ok_or_else(|| std::io::Error::other("file size overflow"))?;
        provider.update(bytes);
        local.update(bytes);
    }
    let actual: [u8; 20] = provider.finalize().into();
    Ok((
        actual == *expected,
        ExistingFileFingerprint {
            size,
            sha256: local.finalize().into(),
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::{compare, supported};
    use sha1::{Digest, Sha1};
    use sha2::Sha256;
    use std::io::Read;
    fn signature(bytes: &[u8]) -> String {
        let mut hash = Sha1::new();
        hash.update(b"com.apple.XattrObjectSalt\0com.apple.DataObjectSalt\0");
        hash.update(bytes);
        data_encoding::BASE64.encode(&[vec![1], hash.finalize().to_vec()].concat())
    }
    #[test]
    fn supported_version_is_explicit_and_other_versions_are_not_guessed() {
        let digest = supported(&signature(b"synthetic")).unwrap();
        assert_eq!(digest.len(), 20);
        for bytes in [vec![4; 21], vec![1; 20], vec![1; 22], vec![]] {
            assert!(supported(&data_encoding::BASE64.encode(&bytes)).is_none());
        }
        assert!(supported("not base64").is_none());
        assert!(supported(&data_encoding::BASE64.encode(&Sha1::digest(b"synthetic"))).is_none());
    }
    #[test]
    fn same_size_and_same_64k_prefix_do_not_hide_a_changed_tail() {
        let bytes = vec![17_u8; 65_537];
        let expected = supported(&signature(&bytes)).unwrap();
        let (matched, fingerprint) = compare(&mut bytes.as_slice(), &expected).unwrap();
        assert!(matched);
        assert_eq!(fingerprint.size, 65_537);
        assert_eq!(fingerprint.sha256, <[u8; 32]>::from(Sha256::digest(&bytes)));
        let mut wrong = bytes;
        *wrong.last_mut().unwrap() ^= 1;
        assert!(!compare(&mut wrong.as_slice(), &expected).unwrap().0);
    }
    #[test]
    fn io_error_is_not_a_missing_or_matching_file() {
        struct Failed;
        impl Read for Failed {
            fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
                Err(std::io::Error::other("synthetic read failure"))
            }
        }
        assert!(compare(&mut Failed, &[0; 20]).is_err());
    }
}

// Independent fixed vectors generated with Python hashlib (OpenSSL), not the
// Rust implementation under test: input is exactly size repetitions of 0x5a.
// Provider hash prepends the documented salt; local SHA-256 does not.
// Include empty/tiny, hash-block, 64KiB, MMCS 5MiB chunk, and large boundaries.
#[cfg(test)]
mod independent_vectors {
    use super::{compare, supported};
    use std::io::Read;
    const VECTORS: &[(u64, &str, &str)] = &[
        (
            0,
            "AW2cWT2PxzWixMKuuNMTHJ8CJ3sH",
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
        ),
        (
            1,
            "AfbJ5DXvmQN9hWAStYpjSzM5BqTF",
            "bbeebd879e1dff6918546dc0c179fdde505f2a21591c9a9c96e36b054ec5af83",
        ),
        (
            2,
            "AXQUVlZGYxiAWND7ttnnnXowIQHw",
            "099987a5188a32ab07b68b4219a824bb83bfcc10aca0fd4f58e41c99b37f09f9",
        ),
        (
            3,
            "AQy+DOQOt7fzOtpExj5Ko3DiT5FA",
            "71f3e86c42376ed1c9583f0117fad889f5139926594992b3f573e17939cb038f",
        ),
        (
            63,
            "AWAQKLUy1lsknN6D/GF7Gvlg+aFu",
            "939765b120205cbedae2ed31256b1967c38b6bdd9b0220535224cbc0b906d333",
        ),
        (
            64,
            "AQ6KLGUnYDFqr/mA857d82ofhbED",
            "cc7321cce5e4409bd8077d58422e1214969059bbd40b4eeb0de0a642f40f7282",
        ),
        (
            65,
            "AdUe8DVoya7UFYhP4m3tMm6E5HAS",
            "b8de0db62b6c87db61345504a8038bf973d987e8d2111abd8beb407c0bf3d9db",
        ),
        (
            65535,
            "ARzIDM3+Js2SxmH4jBQJucTI252X",
            "500d8c05fe096a0bbb4d29a5eab3974790ec52c8e50e6f6b34ba456d4dd47d1e",
        ),
        (
            65536,
            "AYKz2S6fP03ZT7nI1hGUpLEkJysm",
            "944044fe482bc4e91085c15c5a923a1b9e02eac98d3bce04997d6dbecd2a5b8d",
        ),
        (
            65537,
            "ARNbeojXHZCFixtsd9sKk1RHYpGu",
            "866a0d5bb3f1ddb04de4ed7ef911957b0fff959c4bdf17569109727c7c2e1254",
        ),
        (
            5242879,
            "AYnd1j2ySfgeA4WNL4hJWoqneClS",
            "c4439ef09e82c833d71bf70b814e4c293769938a73fad4aa593f4661811f0e01",
        ),
        (
            5242880,
            "AWVtQilrOw7mo+6O+UzRzeyNXRIa",
            "38d3b1d32cdc2f4e84637fd017fe40bc09e794c106fe395e586f776171f7d4ed",
        ),
        (
            5242881,
            "AQFZvtsQUnX41zSxsbxw2c4HQYHf",
            "40e6c18ee8e5726b12cdd76e137b78d4866d1640dd41c345fb3c5d0f9f8d3f8e",
        ),
        (
            10485777,
            "AYYcvT7ekpQMXZYBz7bEhNX477GA",
            "728334225181b7460c9eac3106b7926308e2ab954ea621e7e4676ebdc9426bd7",
        ),
        (
            104857619,
            "Aaw3BnYg9nzDIUdzyqVUscb3vjdQ",
            "aad6577cb75c6866ba74cc72677ccb7dd0d759f9d43bacee253a3008fbdae29b",
        ),
    ];
    #[test]
    fn provider_v1_matches_independent_empty_tiny_chunk_boundary_and_large_vectors() {
        for &(size, provider, local) in VECTORS {
            let expected = supported(provider).unwrap();
            let (matched, actual) =
                compare(&mut std::io::repeat(0x5a).take(size), &expected).unwrap();
            assert!(matched, "independent provider vector failed at size {size}");
            assert_eq!(actual.size, size);
            assert_eq!(data_encoding::HEXLOWER.encode(&actual.sha256), local);
        }
    }

    #[test]
    fn provider_v1_rejects_every_other_version_and_noncanonical_encoding() {
        let (_, valid, _) = VECTORS[1];
        let mut raw = data_encoding::BASE64.decode(valid.as_bytes()).unwrap();
        for version in 0_u8..=255 {
            raw[0] = version;
            assert_eq!(
                supported(&data_encoding::BASE64.encode(&raw)).is_some(),
                version == 1
            );
        }
        for invalid in [
            format!(" {valid}"),
            format!("{valid} "),
            format!(
                "{valid}
"
            ),
            format!("{valid}="),
            valid.get(..valid.len() - 1).unwrap().to_owned(),
            "AQ--________________________".into(),
            "01".repeat(21),
            "é".repeat(21),
        ] {
            assert!(
                supported(&invalid).is_none(),
                "noncanonical signature was accepted"
            );
        }
    }
}
