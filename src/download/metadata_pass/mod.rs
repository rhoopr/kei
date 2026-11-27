//! Metadata writes for copies kei already saved.
//!
//! The sync core decides which copies to visit and records the outcome.
//! This module builds each copy's payload from the catalog, plans the
//! embedded write against what the file already holds, and drives the
//! guarded writers in [`super::metadata`]. An embedded rewrite replaces the
//! media only while both the file and the prepared output still match the
//! fingerprints it was planned from; a sidecar is replaced only while it
//! still matches the writer's initial read.

mod planning;

use std::path::Path;

use chrono::{DateTime, FixedOffset};
use sha2::{Digest, Sha256};

use super::file::ExistingFileFingerprint;
use super::filter::MetadataPayload;
use crate::icloud::photos::PhotoAsset;
use crate::state::types::{EmbeddedMetadataFields, VersionSizeKey};

#[cfg(feature = "xmp")]
use planning::gps_from_payload;
pub(crate) use planning::{CaptureTimestampRepair, MetadataFlags};
use planning::{offset_time_original, plan_metadata_write_with_repair};

/// Which metadata the user enabled, and how to write it.
#[derive(Debug, Clone)]
pub(crate) struct Settings {
    pub(crate) flags: MetadataFlags,
    pub(crate) repair: CaptureTimestampRepair,
    pub(crate) temp_suffix: String,
}

impl Settings {
    /// `None` when no metadata writer is enabled.
    // CONTRACT: METADATA_WRITES_REQUIRE_OPT_IN
    // Without an enabled writer there are no settings, so the pass never runs.
    pub(crate) fn from_config(
        metadata: &crate::config::MetadataConfig,
        repair: CaptureTimestampRepair,
        temp_suffix: &str,
    ) -> Option<Self> {
        let flags = MetadataFlags::from(metadata);
        flags.has_any_write().then(|| Self {
            flags,
            repair,
            temp_suffix: temp_suffix.to_owned(),
        })
    }

    /// Changes here make every saved copy's metadata worth checking again.
    pub(crate) fn fingerprint(&self) -> String {
        format!(
            "{}:{}:{:?}",
            MetadataFlags::POLICY_REVISION,
            self.flags.bits(),
            self.repair
        )
    }

    fn embeds(&self) -> bool {
        self.flags.any_embed()
    }
}

/// What to write for one saved copy.
pub(crate) struct Payload {
    payload: MetadataPayload,
    created_local: DateTime<FixedOffset>,
    /// Identifies the values this payload writes, not the file's state.
    pub(crate) hash: String,
}

/// Build the payload for `rendition` of `asset`. `owned` are the embedded
/// fields kei wrote into this file before; only those may be cleared.
pub(crate) fn payload(
    settings: &Settings,
    asset: &PhotoAsset,
    rendition: &str,
    albums: &[String],
    owned: EmbeddedMetadataFields,
) -> anyhow::Result<Payload> {
    let version = VersionSizeKey::from_str(rendition)
        .ok_or_else(|| anyhow::anyhow!("Unknown rendition {rendition}"))?;
    let metadata = asset.metadata_arc(version);
    let flags = settings.flags;
    let mut payload = MetadataPayload::from_metadata(&metadata).with_asset_groupings(
        albums,
        &[],
        (
            flags.contains(MetadataFlags::INCLUDE_ALBUMS),
            flags.contains(MetadataFlags::INCLUDE_PEOPLE),
        ),
    );
    if flags.contains(MetadataFlags::ORIENTATION) {
        let checksum = asset
            .versions()
            .iter()
            .find(|(key, _)| VersionSizeKey::from(*key) == version)
            .map(|(_, resource)| resource.checksum.to_string())
            .unwrap_or_default();
        payload.original_orientation =
            crate::icloud::photos::metadata::original_orientation(&metadata, version, &checksum);
    }
    let created_local = asset.created_local();
    let mut digest = Sha256::new();
    digest.update(settings.fingerprint().as_bytes());
    digest.update(created_local.to_rfc3339().as_bytes());
    digest.update(serde_json::to_vec(&payload)?);
    payload.prior_embedded = owned;
    Ok(Payload {
        payload,
        created_local,
        hash: format!("{:x}", digest.finalize()),
    })
}

/// How a write ended.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Outcome<T> {
    /// Written; carries the result.
    Written(T),
    /// The file already holds these values.
    Unchanged,
    /// The file changed after it was checked. Nothing was replaced.
    Changed,
    /// The write failed; retry later.
    Failed,
}

/// A prepared embedded rewrite, not yet published.
pub(crate) struct PreparedEmbed {
    prepared: super::metadata::PreparedMetadataFile,
}

impl PreparedEmbed {
    /// SHA-256 of the bytes publication installs.
    pub(crate) fn output_sha256(&self) -> String {
        super::core_io::hex(&self.prepared.output_fingerprint().sha256)
    }

    /// The embedded fields kei owns after publication, as JSON.
    pub(crate) fn fields_json(&self) -> anyhow::Result<String> {
        Ok(serde_json::to_string(&*self.prepared.embedded_fields)?)
    }
}

/// Whether `path`'s format takes embedded metadata at all.
pub(crate) fn embeds_into(settings: &Settings, path: &Path) -> bool {
    settings.embeds() && super::metadata::is_embed_writable_path(path)
}

/// Plan and prepare an embedded rewrite of `path`, which must still have the
/// `expected` bytes. `source_sha256` names the bytes kei downloaded, the only
/// proof that the file holds the untransformed original.
pub(crate) async fn prepare_embed(
    settings: &Settings,
    path: &Path,
    expected: (u64, [u8; 32]),
    source_sha256: Option<&str>,
    payload: &Payload,
) -> Outcome<PreparedEmbed> {
    let path = path.to_path_buf();
    let expected = ExistingFileFingerprint {
        size: expected.0,
        sha256: expected.1,
    };
    let original_bytes =
        source_sha256.is_some_and(|source| super::core_io::hex(&expected.sha256) == source);
    let request = EmbedRequest {
        expected,
        original_bytes,
        payload: payload.payload.clone(),
        created_local: payload.created_local,
        flags: settings.flags,
        repair: settings.repair,
        temp_suffix: settings.temp_suffix.clone(),
    };
    let dispatcher = tracing::dispatcher::get_default(Clone::clone);
    tokio::task::spawn_blocking(move || {
        tracing::dispatcher::with_default(&dispatcher, || prepare_embed_blocking(&path, &request))
    })
    .await
    .unwrap_or_else(|error| {
        tracing::warn!(error = %error, "Metadata preparation task panicked");
        Outcome::Failed
    })
}

struct EmbedRequest {
    expected: ExistingFileFingerprint,
    original_bytes: bool,
    payload: MetadataPayload,
    created_local: DateTime<FixedOffset>,
    flags: MetadataFlags,
    repair: CaptureTimestampRepair,
    temp_suffix: String,
}

fn prepare_embed_blocking(path: &Path, request: &EmbedRequest) -> Outcome<PreparedEmbed> {
    let EmbedRequest {
        expected,
        original_bytes,
        payload,
        created_local,
        flags,
        repair,
        temp_suffix,
    } = request;
    let repair_requested = matches!(repair, CaptureTimestampRepair::ReplaceWithCaptureLocal);
    let repair_offset = repair_requested
        .then(|| offset_time_original(payload))
        .flatten();
    if repair_requested && repair_offset.is_none() {
        tracing::warn!(path = %path.display(), "Capture timestamp repair has no usable offset; leaving the file as it is");
        return Outcome::Failed;
    }
    let mut probe = match super::metadata::probe_exif(path) {
        Ok(probe) => probe,
        Err(error) => {
            crate::support::metadata_error(path, "metadata_probe", &error, Some(false));
            tracing::warn!(path = %path.display(), error = %error, "Failed to read EXIF");
            #[cfg(feature = "xmp")]
            if super::heif::classify_heif_metadata_limit(&error) {
                return Outcome::Failed;
            }
            super::metadata::ExifProbe::default()
        }
    };
    if flags.contains(MetadataFlags::ORIENTATION) && payload.original_orientation.is_some() {
        // A checksum or filename alone does not prove the local pixels are the
        // original resource; require the exact downloaded bytes.
        probe.orientation_missing =
            *original_bytes && super::metadata::original_orientation_missing(path).unwrap_or(false);
    }
    let mut write =
        plan_metadata_write_with_repair(*flags, payload, created_local, *repair, &probe);
    if write.require_native_heif_capture_time {
        let Some(expected_offset) = repair_offset else {
            return Outcome::Failed;
        };
        match probe.native_heif_capture_time_repair_required(created_local, &expected_offset) {
            Ok(Some(_)) => {
                write.datetime = Some(created_local.format("%Y:%m:%d %H:%M:%S").to_string());
                let millis = created_local.timestamp_subsec_millis();
                write.datetime_subseconds = (millis != 0).then(|| format!("{millis:03}"));
                write.offset_time_original = Some(expected_offset);
                write.clear_datetime_offsets = probe.has_any_datetime_offset();
            }
            Ok(None) => {}
            Err(reason) => {
                tracing::warn!(path = %path.display(), reason, "Cannot safely repair the native HEIF capture timestamp; leaving the file as it is");
                return Outcome::Failed;
            }
        }
    }
    crate::support::observe_metadata(
        path,
        "support_metadata_v1",
        serde_json::json!({
            "operation": "prepare",
            "source_fractional": created_local.timestamp_subsec_nanos() != 0,
            "planned_fractional": write.datetime.as_ref().is_some_and(|v| v.contains('.')),
            "timestamp_planned": write.datetime.is_some()}),
    );
    if write.is_empty() {
        return Outcome::Unchanged;
    }
    match super::metadata::prepare_metadata_with_expected_fingerprint(
        path,
        &write,
        temp_suffix,
        Some(*expected),
    ) {
        Ok(prepared) => Outcome::Written(PreparedEmbed { prepared }),
        Err(error) => {
            crate::support::metadata_error(path, "metadata_prepare", &error, Some(false));
            let disposition = super::file::classify_conditional_publish_error(&error);
            tracing::warn!(
                path = %path.display(),
                error = %format!("{error:#}"),
                retained_paths = ?disposition.retained_paths,
                "Failed to prepare metadata"
            );
            if disposition.target_changed {
                Outcome::Changed
            } else {
                Outcome::Failed
            }
        }
    }
}

/// Install a prepared rewrite over `path` while it still has the bytes it
/// was prepared from. Returns the installed bytes' SHA-256.
pub(crate) async fn publish_embed(path: &Path, embed: PreparedEmbed) -> Outcome<String> {
    let target = path.to_path_buf();
    let published = tokio::task::spawn_blocking(move || embed.prepared.publish(&target)).await;
    match published {
        Ok(Ok(output)) => {
            if let Err(error) = crate::fs_util::fsync_parent_dir_async(path).await {
                tracing::warn!(path = %path.display(), error = %error, "Could not sync the folder after a metadata write");
                return Outcome::Failed;
            }
            Outcome::Written(super::core_io::hex(&output.sha256))
        }
        Ok(Err(error)) => {
            crate::support::metadata_error(path, "metadata_publish", &error, None);
            let disposition = super::file::classify_conditional_publish_error(&error);
            tracing::warn!(
                path = %path.display(),
                error = %format!("{error:#}"),
                retained_paths = ?disposition.retained_paths,
                "Failed to publish metadata"
            );
            if disposition.target_changed {
                Outcome::Changed
            } else {
                Outcome::Failed
            }
        }
        Err(error) => {
            tracing::warn!(error = %error, "Metadata publication task panicked");
            Outcome::Failed
        }
    }
}

/// Write or update the `.xmp` sidecar next to `path`. Fields another tool
/// put in an existing sidecar stay; only kei's fields change.
#[cfg(feature = "xmp")]
pub(crate) async fn write_sidecar(
    settings: &Settings,
    path: &Path,
    source_sha256: Option<&str>,
    payload: &Payload,
) -> Outcome<()> {
    let media = path.to_path_buf();
    let source = source_sha256.map(ToOwned::to_owned);
    let plan_payload = payload.payload.clone();
    let created_local = payload.created_local;
    let planned = tokio::task::spawn_blocking(move || {
        plan_sidecar_write(&media, &plan_payload, &created_local, source.as_deref())
    })
    .await;
    let (write, source_error) = match planned {
        Ok(planned) => planned,
        Err(error) => {
            tracing::warn!(error = %error, "XMP sidecar planning task panicked");
            return Outcome::Failed;
        }
    };
    if let Some(error) = &source_error {
        crate::support::metadata_error(path, "source_gps", error, Some(false));
        tracing::warn!(path = %path.display(), error = %format!("{error:#}"), "Could not read the photo's GPS for its sidecar; will retry");
        return Outcome::Failed;
    }
    crate::support::observe_metadata(
        path,
        "support_metadata_v1",
        serde_json::json!({
            "operation": "sidecar",
            "source_fractional": created_local.timestamp_subsec_nanos() != 0,
            "planned_fractional": write.datetime.as_ref().is_some_and(|v| v.contains('.')),
            "timestamp_planned": write.datetime.is_some()}),
    );
    match Box::pin(super::metadata::write_sidecar(
        path,
        &write,
        &settings.temp_suffix,
    ))
    .await
    {
        Ok(true) => Outcome::Written(()),
        Ok(false) => Outcome::Unchanged,
        Err(error) => {
            crate::support::metadata_error(path, "sidecar_write", &error, None);
            tracing::warn!(path = %path.display(), error = %format!("{error:#}"), "Failed to write XMP sidecar");
            Outcome::Failed
        }
    }
}

#[cfg(not(feature = "xmp"))]
pub(crate) async fn write_sidecar(
    _settings: &Settings,
    _path: &Path,
    _source_sha256: Option<&str>,
    _payload: &Payload,
) -> Outcome<()> {
    Outcome::Unchanged
}

/// Every field a payload can contribute to a sidecar. Source GPS facts are
/// read from the media on each attempt. Native horizontal accuracy needs the
/// exact downloaded bytes and matching coordinates.
#[cfg(feature = "xmp")]
fn plan_sidecar_write(
    path: &Path,
    payload: &MetadataPayload,
    created_local: &DateTime<FixedOffset>,
    source_sha256: Option<&str>,
) -> (super::metadata::MetadataWrite, Option<anyhow::Error>) {
    let (source_gps, mut source_error) = match super::metadata::read_source_gps(path) {
        Ok(metadata) => (metadata, None),
        Err(error) => (super::metadata::SourceGpsMetadata::default(), Some(error)),
    };
    // CONTRACT: XMP_GPS_ACCURACY_REQUIRES_MATCHING_LOCATION
    // Exact decoded-coordinate equality is conservative: rounding differences
    // omit accuracy rather than using proximity as evidence of the same fix.
    let accuracy_matches_location =
        source_gps
            .latitude
            .zip(source_gps.longitude)
            .is_some_and(|(latitude, longitude)| {
                payload.latitude == Some(latitude) && payload.longitude == Some(longitude)
            });
    // An earlier embed may have inserted these coordinates, so only the bytes
    // kei downloaded establish native provenance.
    let original_source = accuracy_matches_location
        && source_gps.horizontal_positioning_error.is_some()
        && source_sha256.is_some_and(|expected| {
            match super::file::fingerprint_regular_file_snapshot_blocking(path) {
                Ok(actual) => super::core_io::hex(&actual.fingerprint.sha256) == expected,
                Err(error) => {
                    source_error = Some(error);
                    false
                }
            }
        });
    let mut write = super::metadata::MetadataWrite {
        datetime: Some(created_local.format("%Y-%m-%dT%H:%M:%S%.f").to_string()),
        offset_time_original: offset_time_original(payload),
        gps_datetime: source_gps.datetime,
        gps_speed: source_gps.speed,
        gps_speed_ref: source_gps.speed_ref,
        gps_h_positioning_error: source_gps
            .horizontal_positioning_error
            .filter(|_| original_source),
        preserve_source_gps: source_error.is_some(),
        rating: payload.rating,
        gps: gps_from_payload(payload),
        ..super::metadata::MetadataWrite::default()
    };
    write.title.clone_from(&payload.title);
    write.description.clone_from(&payload.description);
    write.keywords.clone_from(&payload.keywords);
    write.people.clone_from(&payload.people);
    (write, source_error)
}

#[cfg(all(test, feature = "xmp"))]
mod tests {
    use super::*;
    use crate::test_helpers::{exif_with_source_gps, ur64};
    use chrono::TimeZone;

    /// A JPEG whose EXIF fixes 37.7 N, 122.4 W with a 1.5 m horizontal error.
    fn jpeg_with_location() -> Vec<u8> {
        use little_exif::exif_tag::ExifTag;
        let mut metadata = exif_with_source_gps();
        metadata.set_tag(ExifTag::GPSLatitudeRef("N".into()));
        metadata.set_tag(ExifTag::GPSLatitude(vec![
            ur64(37, 1),
            ur64(42, 1),
            ur64(0, 1),
        ]));
        metadata.set_tag(ExifTag::GPSLongitudeRef("W".into()));
        metadata.set_tag(ExifTag::GPSLongitude(vec![
            ur64(122, 1),
            ur64(24, 1),
            ur64(0, 1),
        ]));
        let mut bytes = vec![
            0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46, 0x49, 0x46, 0x00, 0x01, 0x01, 0x00,
            0x00, 0x01, 0x00, 0x01, 0x00, 0x00, 0xFF, 0xD9,
        ];
        metadata
            .write_to_vec(&mut bytes, little_exif::filetype::FileExtension::JPEG)
            .unwrap();
        bytes
    }

    #[test]
    fn contract_metadata_writes_require_opt_in_default_config_writes_nothing() {
        let off = crate::config::MetadataConfig::default();
        assert!(Settings::from_config(&off, CaptureTimestampRepair::Preserve, ".part").is_none());
        let rating = crate::config::MetadataConfig {
            set_exif_rating: true,
            ..crate::config::MetadataConfig::default()
        };
        let settings =
            Settings::from_config(&rating, CaptureTimestampRepair::Preserve, ".part").unwrap();
        assert!(settings.flags.contains(MetadataFlags::RATING));
        // Album names alone enrich a writer; they do not enable one.
        let albums = crate::config::MetadataConfig {
            include_albums: true,
            ..crate::config::MetadataConfig::default()
        };
        assert!(
            Settings::from_config(&albums, CaptureTimestampRepair::Preserve, ".part").is_none()
        );
    }

    #[test]
    fn contract_xmp_gps_accuracy_requires_matching_location_and_downloaded_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("photo.jpg");
        let bytes = jpeg_with_location();
        std::fs::write(&path, &bytes).unwrap();
        let downloaded = super::super::core_io::hex(&{
            use sha2::Digest;
            Sha256::digest(&bytes).into()
        });
        let source = super::super::metadata::read_source_gps(&path).unwrap();
        let payload = |latitude, longitude| MetadataPayload {
            latitude: Some(latitude),
            longitude: Some(longitude),
            ..MetadataPayload::default()
        };
        let created = FixedOffset::east_opt(0)
            .unwrap()
            .with_ymd_and_hms(2024, 6, 15, 10, 0, 0)
            .unwrap();
        let (latitude, longitude) = (source.latitude.unwrap(), source.longitude.unwrap());

        let (write, error) = plan_sidecar_write(
            &path,
            &payload(latitude, longitude),
            &created,
            Some(&downloaded),
        );
        assert!(error.is_none());
        assert!(write.gps_h_positioning_error.is_some());

        // Coordinates that differ from the file's fix carry no accuracy.
        let (write, _) = plan_sidecar_write(
            &path,
            &payload(latitude + 0.001, longitude),
            &created,
            Some(&downloaded),
        );
        assert!(write.gps_h_positioning_error.is_none());

        // Without the downloaded bytes, kei cannot tell its own embed from the camera's.
        let (write, _) = plan_sidecar_write(&path, &payload(latitude, longitude), &created, None);
        assert!(write.gps_h_positioning_error.is_none());
        let (write, _) = plan_sidecar_write(
            &path,
            &payload(latitude, longitude),
            &created,
            Some(&"0".repeat(64)),
        );
        assert!(write.gps_h_positioning_error.is_none());
    }
}
