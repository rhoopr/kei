//! Resolved runtime policy types and date-bound semantics.

use crate::password::SecretString;
use crate::types::{
    Domain, FileMatchPolicy, LivePhotoMode, LivePhotoMovFilenamePolicy, LivePhotoResolution,
    PhotoResolution, RawPolicy,
};
use chrono::{DateTime, Local, NaiveDate, NaiveDateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum MediaKind {
    Photos,
    Videos,
    LivePhotos,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct MediaSelection {
    pub photos: bool,
    pub videos: bool,
    pub live_photos: bool,
}

impl Default for MediaSelection {
    fn default() -> Self {
        Self::all()
    }
}

impl MediaSelection {
    pub(crate) const fn all() -> Self {
        Self {
            photos: true,
            videos: true,
            live_photos: true,
        }
    }

    pub(crate) const fn skip_videos(self) -> bool {
        !self.videos
    }

    pub(crate) const fn skip_photos(self) -> bool {
        !self.photos
    }

    pub(crate) const fn is_all(self) -> bool {
        self.photos && self.videos && self.live_photos
    }

    pub(crate) fn to_kinds(self) -> Vec<MediaKind> {
        [
            (self.photos, MediaKind::Photos),
            (self.videos, MediaKind::Videos),
            (self.live_photos, MediaKind::LivePhotos),
        ]
        .into_iter()
        .filter_map(|(enabled, kind)| enabled.then_some(kind))
        .collect()
    }
}

#[derive(Debug)]
pub struct AuthConfig {
    pub username: String,
    pub password: Option<SecretString>,
    pub password_file: Option<PathBuf>,
    pub password_command: Option<String>,
    pub cookie_directory: PathBuf,
    pub domain: Domain,
    pub save_password: bool,
}

#[derive(Debug)]
pub struct DownloadSettings {
    pub directory: PathBuf,
    pub folder_structure: String,
    pub folder_structure_albums: String,
    pub folder_structure_smart_folders: String,
    pub filename_exclude: Vec<glob::Pattern>,
    pub temp_suffix: String,
    pub threads_num: u16,
    pub bandwidth_limit: Option<u64>,
    pub no_progress_bar: bool,
}

#[derive(Debug)]
pub struct FilterConfig {
    pub selection: crate::selection::Selection,
    pub media: MediaSelection,
    pub skip_created_before: Option<CreatedDateFilter>,
    pub skip_created_after: Option<CreatedDateFilter>,
    pub recent: Option<u32>,
    pub recent_scope: crate::cli::RecentScope,
    pub persistent_recent: Option<crate::cli::RecentLimit>,
    pub persistent_recent_scope: Option<crate::cli::RecentScope>,
    pub persistent_skip_created_before: Option<String>,
    pub persistent_skip_created_after: Option<String>,
    pub skip_videos: bool,
    pub skip_photos: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CreatedDateFilter {
    Instant(DateTime<Utc>),
    CaptureDate(NaiveDate),
}

impl CreatedDateFilter {
    pub(crate) fn excludes_before(
        self,
        created_at: DateTime<Utc>,
        capture_date: NaiveDate,
    ) -> bool {
        match self {
            Self::Instant(boundary) => created_at < boundary,
            Self::CaptureDate(boundary) => capture_date < boundary,
        }
    }

    pub(crate) fn excludes_after(self, created_at: DateTime<Utc>, capture_date: NaiveDate) -> bool {
        match self {
            Self::Instant(boundary) => created_at > boundary,
            Self::CaptureDate(boundary) => capture_date > boundary,
        }
    }

    /// True when the two bounds admit no asset at all, whatever capture
    /// offset it carries.
    ///
    /// Bounds of the same form compare exactly. A capture date names a span of
    /// instants rather than one, because the filter compares each asset's
    /// capture-local date, so a mixed window is judged against the widest span
    /// `FixedOffset` admits. Reporting a workable window as impossible would
    /// be worse than staying quiet about a narrow contradictory one.
    pub(crate) fn is_strictly_after(self, other: Self) -> bool {
        match (self, other) {
            (Self::Instant(before), Self::Instant(after)) => before > after,
            (Self::CaptureDate(before), Self::CaptureDate(after)) => before > after,
            (Self::CaptureDate(before), Self::Instant(after)) => {
                Self::earliest_instant_on(before) > after
            }
            (Self::Instant(before), Self::CaptureDate(after)) => {
                Self::first_instant_after(after).is_some_and(|end| before >= end)
            }
        }
    }

    /// Widest offset `chrono::FixedOffset` accepts, in seconds. A capture-local
    /// calendar day therefore spans this much beyond the UTC day on each side.
    const WIDEST_OFFSET_SECONDS: i64 = 86_399;

    /// Earliest instant an asset can carry and still fall on `date` in
    /// capture-local time.
    fn earliest_instant_on(date: NaiveDate) -> DateTime<Utc> {
        date.and_time(chrono::NaiveTime::MIN).and_utc()
            - chrono::Duration::seconds(Self::WIDEST_OFFSET_SECONDS)
    }

    /// First instant that can no longer fall on `date` in capture-local time.
    /// `None` only at the end of the representable calendar, where declining
    /// to warn is the safe answer.
    fn first_instant_after(date: NaiveDate) -> Option<DateTime<Utc>> {
        Some(
            date.succ_opt()?.and_time(chrono::NaiveTime::MIN).and_utc()
                + chrono::Duration::seconds(Self::WIDEST_OFFSET_SECONDS),
        )
    }

    /// Midnight UTC on a capture date, or the instant itself.
    fn comparable_instant(self) -> DateTime<Utc> {
        match self {
            Self::Instant(boundary) => boundary,
            Self::CaptureDate(boundary) => boundary.and_time(chrono::NaiveTime::MIN).and_utc(),
        }
    }

    pub(crate) fn conservative_utc_lower_bound(self) -> DateTime<Utc> {
        match self {
            Self::Instant(boundary) => boundary,
            Self::CaptureDate(_) => self.comparable_instant() - chrono::Duration::days(1),
        }
    }

    /// Durable identity used by the config and coverage hashes. Keep it
    /// separate from [`std::fmt::Display`], so rewording a diagnostic cannot
    /// invalidate every user's incremental state.
    pub(crate) fn fingerprint(self) -> String {
        match self {
            Self::Instant(boundary) => boundary.to_rfc3339(),
            Self::CaptureDate(boundary) => format!("capture-date:{boundary}"),
        }
    }
}

/// Diagnostic rendering only. Each bound names its form, because the two forms
/// compare differently and a message about a contradictory window is only
/// readable once the reader knows which one they gave.
impl std::fmt::Display for CreatedDateFilter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Instant(boundary) => write!(
                f,
                "instant {}",
                boundary.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
            ),
            Self::CaptureDate(boundary) => write!(f, "capture date {boundary}"),
        }
    }
}

impl FilterConfig {
    /// True when the resolved filters narrow a sync below a full sweep of the
    /// selected libraries, so a `--refresh-metadata` repair cannot revisit
    /// every downloaded asset in scope.
    pub(crate) fn narrows_enumeration(&self) -> bool {
        self.recent.is_some()
            || self.skip_created_before.is_some()
            || self.skip_created_after.is_some()
            || !self.media.is_all()
            || self.selection.albums_explicit
            || self.selection.smart_folders_explicit
            || !self.selection.unfiled
    }
}

#[derive(Debug)]
pub struct PhotoConfig {
    pub resolution: PhotoResolution,
    pub live_resolution: LivePhotoResolution,
    pub live_photo_mode: LivePhotoMode,
    pub live_photo_mov_filename_policy: LivePhotoMovFilenamePolicy,
    pub edited: bool,
    pub alternative: bool,
    pub raw_policy: RawPolicy,
    pub file_match_policy: FileMatchPolicy,
    pub force_resolution: bool,
    pub keep_unicode_in_filenames: bool,
}

#[derive(Debug)]
pub struct ResolvedRetryConfig {
    pub max_retries: u32,
    pub retry_delay_secs: u64,
    pub max_download_attempts: u32,
}

#[derive(Debug)]
pub struct WatchConfig {
    pub interval: Option<u64>,
    pub notify_systemd: bool,
    pub pid_file: Option<PathBuf>,
    pub reconcile_every_n_cycles: Option<u64>,
}

#[derive(Debug)]
pub struct NotificationConfig {
    pub script: Option<PathBuf>,
}

#[derive(Debug)]
pub struct ReportConfig {
    pub json: Option<PathBuf>,
}

#[derive(Debug)]
pub struct ServerConfig {
    pub port: u16,
    pub bind: std::net::IpAddr,
}

#[derive(Debug)]
pub struct UiConfig {
    pub personality_mode: crate::personality::Mode,
    pub friendly_request: Option<bool>,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct MetadataConfig {
    pub set_exif_datetime: bool,
    pub set_exif_rating: bool,
    pub set_exif_gps: bool,
    pub set_exif_description: bool,
    /// Embed the full XMP packet into the file bytes on supported formats.
    #[cfg(feature = "xmp")]
    pub embed_xmp: bool,
    /// Write a `.xmp` sidecar file next to each downloaded media file.
    #[cfg(feature = "xmp")]
    pub xmp_sidecar: bool,
}

#[derive(Debug)]
pub struct ImportConfig {
    pub strict: bool,
}

#[derive(Debug)]
pub struct RuntimeConfig {
    pub dry_run: bool,
    pub only_print_filenames: bool,
    pub refresh_metadata: bool,
    pub repair_capture_timestamps: bool,
    pub repair_truncated: bool,
}

/// Application configuration.
///
/// Fields are ordered for optimal memory layout:
/// - Heap types first (String, `PathBuf`, Vec, `Option<String>`)
/// - `DateTime` fields (12-16 bytes each)
/// - 8-byte primitives (u64, `Option<u64>`)
/// - 4-byte primitives (u32, `Option<u32>`)
/// - 2-byte primitives (u16)
/// - 1-byte enums
/// - All booleans grouped at the end
pub struct Config {
    pub auth: AuthConfig,
    pub download: DownloadSettings,
    pub filters: FilterConfig,
    pub photos: PhotoConfig,
    pub retry: ResolvedRetryConfig,
    pub watch: WatchConfig,
    pub notifications: NotificationConfig,
    pub report: ReportConfig,
    pub server: ServerConfig,
    pub ui: UiConfig,
    pub metadata: MetadataConfig,
    pub import: ImportConfig,
    pub runtime: RuntimeConfig,
}

impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("username", &self.auth.username)
            .field("password", &"<redacted>")
            .field("directory", &self.download.directory)
            .field("domain", &self.auth.domain)
            .field("cookie_directory", &self.auth.cookie_directory)
            .field("metadata", &self.metadata)
            .field("import", &self.import)
            .finish_non_exhaustive()
    }
}

/// Parse a date-only capture boundary, host-local datetime, or relative interval.
pub(crate) fn parse_created_date_filter(s: &str) -> anyhow::Result<CreatedDateFilter> {
    if let Some(days_str) = s.strip_suffix('d')
        && let Ok(days) = days_str.parse::<u64>()
    {
        let days = i64::try_from(days)
            .map_err(|_e| anyhow::anyhow!("Date interval '{s}' is too large"))?;
        return Ok(CreatedDateFilter::Instant(
            Utc::now() - chrono::Duration::days(days),
        ));
    }
    if let Ok(date) = NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        return Ok(CreatedDateFilter::CaptureDate(date));
    }
    if let Ok(dt) = NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S")
        && let Some(local) = dt.and_local_timezone(Local).single()
    {
        return Ok(CreatedDateFilter::Instant(local.with_timezone(&Utc)));
    }
    anyhow::bail!(
        "Could not parse '{s}' as a date. Use a date like 2025-01-02, a datetime like 2025-01-02T14:30:00, or an interval like 20d."
    )
}

#[cfg(test)]
mod tests {
    use super::{Config, CreatedDateFilter, parse_created_date_filter};
    use crate::config::input::TomlConfig;
    use crate::config::test_support::{default_globals, default_password, default_sync};
    use chrono::{Local, NaiveDate, Utc};

    #[test]
    fn test_parse_date_marks_calendar_boundaries() {
        assert_eq!(
            parse_created_date_filter("2025-01-15").unwrap(),
            CreatedDateFilter::CaptureDate(NaiveDate::from_ymd_opt(2025, 1, 15).unwrap())
        );
        assert!(matches!(
            parse_created_date_filter("2025-01-15T00:00:00").unwrap(),
            CreatedDateFilter::Instant(_)
        ));
    }
    #[test]
    fn date_only_filter_bound_is_host_timezone_independent() {
        assert_eq!(
            parse_created_date_filter("2025-01-15")
                .unwrap()
                .fingerprint(),
            "capture-date:2025-01-15"
        );
    }
    /// The contradictory-window warning reads back to whoever wrote the
    /// config, so each bound has to name the form it took rather than leaning
    /// on Rust's derived `Debug`.
    #[test]
    fn filter_bounds_render_their_form_for_diagnostics() {
        assert_eq!(
            CreatedDateFilter::CaptureDate(NaiveDate::from_ymd_opt(2025, 6, 1).unwrap())
                .to_string(),
            "capture date 2025-06-01"
        );
        assert_eq!(
            CreatedDateFilter::Instant(
                NaiveDate::from_ymd_opt(2025, 1, 1)
                    .unwrap()
                    .and_time(chrono::NaiveTime::MIN)
                    .and_utc()
            )
            .to_string(),
            "instant 2025-01-01T00:00:00Z"
        );
    }
    #[test]
    fn conservative_utc_lower_bound_covers_filter_forms_and_maximum_offsets() {
        let capture_date = NaiveDate::from_ymd_opt(2025, 2, 1).unwrap();
        let utc_midnight = capture_date.and_time(chrono::NaiveTime::MIN).and_utc();
        let instant = utc_midnight + chrono::Duration::hours(6);

        for (name, filter, expected) in [
            (
                "capture date",
                CreatedDateFilter::CaptureDate(capture_date),
                utc_midnight - chrono::Duration::days(1),
            ),
            ("instant", CreatedDateFilter::Instant(instant), instant),
        ] {
            assert_eq!(filter.conservative_utc_lower_bound(), expected, "{name}");
        }

        let lower_bound =
            CreatedDateFilter::CaptureDate(capture_date).conservative_utc_lower_bound();
        for (name, offset_seconds, expected_clearance) in [
            ("UTC+14", 50_400, chrono::Duration::hours(10)),
            ("chrono east limit", 86_399, chrono::Duration::seconds(1)),
        ] {
            let offset = chrono::FixedOffset::east_opt(offset_seconds).unwrap();
            let asset_utc = capture_date
                .and_time(chrono::NaiveTime::MIN)
                .and_local_timezone(offset)
                .single()
                .unwrap()
                .with_timezone(&Utc);
            assert_eq!(asset_utc - lower_bound, expected_clearance, "{name}");
            assert!(asset_utc >= lower_bound, "{name} asset was truncated");
        }
    }
    #[test]
    fn test_parse_datetime_iso() {
        let CreatedDateFilter::Instant(dt) =
            parse_created_date_filter("2025-06-15T14:30:00").unwrap()
        else {
            panic!("datetime must resolve to an instant");
        };
        let naive = dt.with_timezone(&Local).naive_local();
        assert_eq!(naive.date(), NaiveDate::from_ymd_opt(2025, 6, 15).unwrap());
        assert_eq!(
            naive.time(),
            chrono::NaiveTime::from_hms_opt(14, 30, 0).unwrap()
        );
    }
    #[test]
    fn test_parse_interval_days() {
        let before = Utc::now();
        let CreatedDateFilter::Instant(dt) = parse_created_date_filter("10d").unwrap() else {
            panic!("relative interval must resolve to an instant");
        };
        let after = Utc::now();
        let expected = before - chrono::Duration::days(10);
        assert!(dt >= expected - chrono::Duration::seconds(1));
        assert!(dt <= after - chrono::Duration::days(10) + chrono::Duration::seconds(1));
    }
    #[test]
    fn test_parse_invalid_date() {
        assert!(parse_created_date_filter("not-a-date").is_err());
        assert!(parse_created_date_filter("").is_err());
    }
    #[test]
    fn test_parse_negative_interval_rejected() {
        assert!(parse_created_date_filter("-5d").is_err());
        assert!(parse_created_date_filter("-1d").is_err());
    }
    #[test]
    fn narrows_enumeration_false_for_default_full_sweep() {
        let cfg = Config::build(
            &default_globals(),
            &default_password(),
            default_sync(),
            None,
        )
        .unwrap();
        assert!(!cfg.filters.narrows_enumeration());
    }
    #[test]
    fn narrows_enumeration_true_for_recent() {
        let toml: TomlConfig = toml::from_str("[filters]\nrecent = 500\n").unwrap();
        let cfg = Config::build(
            &default_globals(),
            &default_password(),
            default_sync(),
            Some(&toml),
        )
        .unwrap();
        assert!(cfg.filters.narrows_enumeration());
    }
    #[test]
    fn narrows_enumeration_true_for_date_window() {
        let toml: TomlConfig =
            toml::from_str("[filters]\nskip_created_before = \"2024-01-01\"\n").unwrap();
        let cfg = Config::build(
            &default_globals(),
            &default_password(),
            default_sync(),
            Some(&toml),
        )
        .unwrap();
        assert!(cfg.filters.narrows_enumeration());
    }
    #[test]
    fn narrows_enumeration_true_for_media_subset() {
        let toml: TomlConfig = toml::from_str("[filters]\nmedia = [\"photos\"]\n").unwrap();
        let cfg = Config::build(
            &default_globals(),
            &default_password(),
            default_sync(),
            Some(&toml),
        )
        .unwrap();
        assert!(cfg.filters.narrows_enumeration());
    }
    #[test]
    fn narrows_enumeration_true_for_album_selection() {
        let toml: TomlConfig = toml::from_str("[filters]\nalbums = [\"Vacation\"]\n").unwrap();
        let cfg = Config::build(
            &default_globals(),
            &default_password(),
            default_sync(),
            Some(&toml),
        )
        .unwrap();
        assert!(cfg.filters.narrows_enumeration());
    }
    #[test]
    fn narrows_enumeration_true_without_unfiled_pass() {
        let toml: TomlConfig = toml::from_str("[filters]\nunfiled = false\n").unwrap();
        let cfg = Config::build(
            &default_globals(),
            &default_password(),
            default_sync(),
            Some(&toml),
        )
        .unwrap();
        assert!(cfg.filters.narrows_enumeration());
    }
    #[test]
    fn narrows_enumeration_true_for_smart_folder_selection() {
        let toml: TomlConfig =
            toml::from_str("[filters]\nsmart_folders = [\"Favorites\"]\n").unwrap();
        let cfg = Config::build(
            &default_globals(),
            &default_password(),
            default_sync(),
            Some(&toml),
        )
        .unwrap();
        assert!(cfg.filters.narrows_enumeration());
    }
    #[test]
    fn contradictory_window_detection_spans_date_and_instant_bounds() {
        let june = CreatedDateFilter::CaptureDate(
            NaiveDate::from_ymd_opt(2025, 6, 1).expect("valid date"),
        );
        let january = CreatedDateFilter::Instant(
            NaiveDate::from_ymd_opt(2025, 1, 1)
                .expect("valid date")
                .and_time(chrono::NaiveTime::MIN)
                .and_utc(),
        );

        assert!(!june.is_strictly_after(june), "one capture day is a window");
        assert!(june.is_strictly_after(january));
        assert!(!january.is_strictly_after(june));

        // An asset sitting exactly on an instant bound satisfies both
        // predicates, so equal instants are a window too.
        assert!(!january.is_strictly_after(january));

        // A capture date spans the instants any offset can map onto it, so a
        // mixed window under a day apart still admits assets. Reporting one of
        // these as impossible would contradict what the filter actually does.
        let capture_day = NaiveDate::from_ymd_opt(2025, 2, 1).expect("valid date");
        let instant_at = |year, month, day, hour| {
            CreatedDateFilter::Instant(
                NaiveDate::from_ymd_opt(year, month, day)
                    .expect("valid date")
                    .and_hms_opt(hour, 0, 0)
                    .expect("valid time")
                    .and_utc(),
            )
        };

        // 2025-02-01T20:00Z is 2025-02-01 locally at UTC+00.
        assert!(
            !instant_at(2025, 2, 1, 20)
                .is_strictly_after(CreatedDateFilter::CaptureDate(capture_day)),
            "a same-day capture-local asset matches this window"
        );
        // 2025-01-31T20:00Z is 2025-02-01 locally at UTC+14.
        assert!(
            !CreatedDateFilter::CaptureDate(capture_day)
                .is_strictly_after(instant_at(2025, 1, 31, 20)),
            "an eastward offset pulls this asset onto the capture day"
        );

        // The diagnostic still fires once no offset can bridge the gap.
        assert!(
            instant_at(2025, 6, 1, 0).is_strictly_after(CreatedDateFilter::CaptureDate(
                NaiveDate::from_ymd_opt(2025, 1, 1).expect("valid date")
            ))
        );
    }
}
