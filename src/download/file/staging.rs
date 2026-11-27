//! Stable staging identity and destination lifetime coordination.

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

fn destination_digest(path: &Path) -> anyhow::Result<Sha256> {
    let path = crate::fs_util::absolute_confined_path(path)?;
    let mut hash = Sha256::new();
    hash.update(b"kei-download-destination-v1\0");
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let bytes = path.as_os_str().as_bytes();
        #[cfg(target_os = "macos")]
        hash.update(bytes.to_ascii_lowercase());
        #[cfg(not(target_os = "macos"))]
        hash.update(bytes);
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        for unit in path.as_os_str().encode_wide() {
            // Match the filesystem owner's ASCII case normalization without
            // converting through a lossy Unicode string.
            let unit = if (u16::from(b'A')..=u16::from(b'Z')).contains(&unit) {
                unit + u16::from(b'a' - b'A')
            } else {
                unit
            };
            hash.update(unit.to_le_bytes());
        }
    }
    Ok(hash)
}

pub(super) fn staging_path(path: &Path, checksum: &[u8], suffix: &str) -> anyhow::Result<PathBuf> {
    let destination = data_encoding::BASE32_NOPAD.encode(&destination_digest(path)?.finalize());
    let mut generation = Sha256::new();
    generation.update(b"kei-provider-generation-v1\0");
    generation.update(checksum);
    let generation = data_encoding::BASE32_NOPAD.encode(&generation.finalize());
    Ok(path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(format!("kei-v1-{destination}-{generation}{suffix}")))
}

#[cfg(test)]
mod tests;
