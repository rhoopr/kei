//! Integrity checks run from both the checkout and the extracted source package.

use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};

use anyhow::{Result, ensure};
use serde::Deserialize;
use sha2::{Digest, Sha256};

#[derive(Deserialize)]
struct Manifest {
    schema_version: u32,
    max_total_bytes: u64,
    files: Vec<Fixture>,
}

#[derive(Deserialize)]
struct Fixture {
    path: String,
    bytes: u64,
    sha256: String,
    origin: String,
    license: String,
    license_file: String,
    recipe: Option<String>,
    source_url: Option<String>,
    coverage: String,
}

fn data_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data")
}

fn manifest() -> Result<Manifest> {
    Ok(serde_json::from_slice(&std::fs::read(
        data_dir().join("media-manifest.json"),
    )?)?)
}

fn verify(root: &Path, manifest: &Manifest) -> Result<()> {
    ensure!(
        manifest.schema_version == 1,
        "unknown fixture manifest schema"
    );
    ensure!(!manifest.files.is_empty(), "fixture manifest is empty");
    let mut total = 0_u64;
    let mut paths = BTreeSet::new();
    for fixture in &manifest.files {
        ensure!(
            Path::new(&fixture.path)
                .components()
                .all(|c| matches!(c, Component::Normal(_))),
            "fixture paths must stay within the corpus"
        );
        ensure!(paths.insert(&fixture.path), "duplicate fixture path");
        ensure!(
            !fixture.origin.is_empty()
                && !fixture.license.is_empty()
                && !fixture.coverage.is_empty(),
            "fixture provenance, licence and coverage are required"
        );
        let bytes = std::fs::read(root.join(&fixture.path))?;
        ensure!(
            bytes.len() as u64 == fixture.bytes,
            "{}: size mismatch",
            fixture.path
        );
        ensure!(
            data_encoding::HEXLOWER.encode(&Sha256::digest(&bytes)) == fixture.sha256,
            "{}: SHA-256 mismatch",
            fixture.path
        );
        total += fixture.bytes;
    }
    ensure!(
        total <= manifest.max_total_bytes,
        "fixture corpus exceeds size budget"
    );
    Ok(())
}

#[test]
fn bundled_fixture_manifest_hashes_and_budget() -> Result<()> {
    let manifest = manifest()?;
    verify(&data_dir(), &manifest)?;
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    for fixture in &manifest.files {
        ensure!(
            !std::fs::read(data_dir().join(&fixture.license_file))?.is_empty(),
            "{}: licence text is missing",
            fixture.path
        );
        if let Some(recipe) = &fixture.recipe {
            ensure!(
                !std::fs::read(root.join(recipe))?.is_empty(),
                "{}: generation recipe is missing",
                fixture.path
            );
        } else {
            ensure!(
                fixture
                    .source_url
                    .as_ref()
                    .is_some_and(|url| url.starts_with("https://")),
                "{}: upstream source is missing",
                fixture.path
            );
        }
    }
    for path in [
        "tests/data/README.md",
        "tests/data/media/encoders.json",
        "tests/data/media/sanitization.json",
        "scripts/fixtures/sanitize_live_photo.py",
        "scripts/fixtures/sanitize_icloud_still.py",
        "tests/data/media/icloud-sanitization.json",
        "scripts/fixtures/check-package.sh",
    ] {
        ensure!(
            !std::fs::read(root.join(path))?.is_empty(),
            "{path}: missing package evidence"
        );
    }
    let tracked: BTreeSet<_> = manifest.files.iter().map(|f| f.path.as_str()).collect();
    for entry in std::fs::read_dir(data_dir().join("media"))? {
        let path = entry?.path();
        if path
            .extension()
            .is_some_and(|extension| extension != "json")
        {
            let relative = path
                .strip_prefix(data_dir())?
                .to_string_lossy()
                .replace('\\', "/");
            ensure!(
                tracked.contains(relative.as_str()),
                "unmanifested media: {relative}"
            );
        }
    }
    Ok(())
}

#[test]
fn bundled_fixture_verifier_rejects_corruption_and_empty_manifest() -> Result<()> {
    let mut manifest = manifest()?;
    let dir = tempfile::tempdir()?;
    let fixture = manifest
        .files
        .first()
        .ok_or_else(|| anyhow::anyhow!("empty manifest"))?;
    let mut bytes = std::fs::read(data_dir().join(&fixture.path))?;
    *bytes
        .first_mut()
        .ok_or_else(|| anyhow::anyhow!("empty media"))? ^= 1;
    let path = dir.path().join(&fixture.path);
    std::fs::create_dir_all(path.parent().ok_or_else(|| anyhow::anyhow!("no parent"))?)?;
    std::fs::write(path, bytes)?;
    let error = verify(dir.path(), &manifest)
        .err()
        .ok_or_else(|| anyhow::anyhow!("corrupt fixture passed verification"))?;
    ensure!(
        error.to_string().contains("SHA-256 mismatch"),
        "wrong corruption diagnostic"
    );
    manifest.files.clear();
    ensure!(
        verify(dir.path(), &manifest).is_err(),
        "empty manifest accepted"
    );
    Ok(())
}

#[test]
fn bundled_live_photo_pair_retains_matching_synthetic_identifier() -> Result<()> {
    let identifier = b"76300000-0000-4000-8000-000000000001";
    for name in ["apple-live.heic", "apple-live.mov"] {
        let bytes = std::fs::read(data_dir().join("media").join(name))?;
        ensure!(
            bytes
                .windows(identifier.len())
                .any(|window| window == identifier),
            "{name}"
        );
    }
    Ok(())
}
