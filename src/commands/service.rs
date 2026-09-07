use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use anyhow::Context;

use crate::auth;
use crate::icloud;
use crate::retry;

/// Maximum number of re-authentication attempts before giving up.
pub(crate) const MAX_REAUTH_ATTEMPTS: u32 = 3;

/// iCloud web-client build identifiers sent with every CloudKit API request.
/// Apple embeds these in the JS bundle served by `icloud.com`. To find updated
/// values: open `icloud.com/photos` in a browser, inspect any CloudKit XHR, and
/// read `clientBuildNumber` / `clientMasteringNumber` from the query string.
const ICLOUD_CLIENT_BUILD_NUMBER: &str = "2522Project44";
const ICLOUD_CLIENT_MASTERING_NUMBER: &str = "2522B2";

/// Initialize the photos service with one 421 recovery attempt.
///
/// On 421 Misdirected Request, resets the HTTP/2 connection pool and retries
/// once. A second 421 surfaces `ICloudError::MisdirectedRequest` to the
/// caller; `sync_loop` routes both 421 and 401 through the same SRP re-auth
/// path (covering the case where stale session routing headers are pinning
/// the request to the wrong partition).
///
/// `mode` controls friendly-mode recovery narration if the retry succeeds;
/// off-mode callers see the existing `tracing::warn!` events unchanged.
pub(crate) async fn init_photos_service(
    mut auth_result: auth::AuthResult,
    api_retry_config: retry::RetryConfig,
    mode: crate::personality::Mode,
    capture: Option<Arc<crate::icloud::photos::capture::ResponseCapture>>,
) -> anyhow::Result<(auth::SharedSession, icloud::photos::PhotosService)> {
    if auth_result.data.i_cdp_enabled {
        anyhow::bail!(
            "Advanced Data Protection (ADP) is enabled on this account.\n\n\
             ADP blocks the web API that kei uses to access photos.\n\
             To use kei, change both settings on your iPhone/iPad:\n  \
             1. Disable ADP: Settings > Apple ID > iCloud > Advanced Data Protection\n  \
             2. Enable web access: Settings > Apple ID > iCloud > Access iCloud Data on the Web"
        );
    }

    let ckdatabasews_url = auth_result
        .data
        .webservices
        .as_ref()
        .and_then(|ws| ws.ckdatabasews.as_ref())
        .map(|ep| ep.url.clone())
        .ok_or_else(|| anyhow::anyhow!("Apple did not return the CloudKit Photos service URL."))?;

    // Persist the active ckdatabasews URL so validate_session can detect
    // partition changes during watch-mode revalidation.
    auth_result
        .session
        .session_data
        .insert("ckdatabasews_url".to_owned(), ckdatabasews_url.clone());

    let client_id = auth_result
        .session
        .client_id()
        .unwrap_or_default()
        .to_owned();
    let dsid = auth_result
        .data
        .ds_info
        .as_ref()
        .and_then(|ds| ds.dsid.clone());
    let params = build_photos_params(&client_id, dsid.as_deref());

    let shared_session: auth::SharedSession =
        std::sync::Arc::new(tokio::sync::RwLock::new(auth_result.session));
    let make_session = || -> Box<dyn icloud::photos::PhotosSession> {
        match &capture {
            Some(capture) => Box::new(icloud::photos::session::CapturingPhotosSession {
                session: Arc::clone(&shared_session),
                capture: Arc::clone(capture),
            }),
            None => Box::new(Arc::clone(&shared_session)),
        }
    };

    tracing::debug!("Initializing photos service...");
    match icloud::photos::PhotosService::new(
        ckdatabasews_url.clone(),
        make_session(),
        params.clone(),
        api_retry_config,
    )
    .await
    {
        Ok(service) => return Ok((shared_session, service)),
        Err(e) if !is_misdirected_request(&e) => return Err(e.into()),
        Err(_) => {}
    }

    // 421 Misdirected Request: Apple's CDN routed our HTTP/2 connection to
    // the wrong CloudKit partition. Per RFC 9110, the correct response is a
    // fresh connection — not re-auth. Try that once; if the second attempt
    // also 421s, surface `MisdirectedRequest` so `sync_loop` can invalidate
    // the cache and force SRP (where stale routing headers are the likely
    // cause).
    tracing::warn!(
        url = %ckdatabasews_url,
        "Service returned 421 Misdirected Request, retrying with fresh connection pool"
    );
    {
        let mut session = shared_session.write().await;
        session.reset_http_clients()?;
    }

    let service = match icloud::photos::PhotosService::new(
        ckdatabasews_url.clone(),
        make_session(),
        params,
        api_retry_config,
    )
    .await
    {
        Ok(s) => {
            crate::personality::narration::back_on_track_to_stderr(mode);
            s
        }
        Err(e) => {
            // The pool-reset retry also failed. Surface it before bubbling
            // so watch-mode operators can correlate reauth cycles with
            // sustained CloudKit partition issues.
            tracing::warn!(
                url = %ckdatabasews_url,
                attempt = 2,
                error = %e,
                "init_photos_service retry after pool reset failed; \
                 sync_loop will fall through to full SRP re-auth"
            );
            return Err(e.into());
        }
    };
    Ok((shared_session, service))
}

/// Check if an iCloud error is a 421 Misdirected Request from the CloudKit service.
///
/// This happens when the HTTP/2 connection is routed to a CloudKit partition
/// server that cannot serve the user's data. Root cause may be stale
/// connection routing or stale session state; see `init_photos_service`.
fn is_misdirected_request(err: &icloud::error::ICloudError) -> bool {
    matches!(err, icloud::error::ICloudError::MisdirectedRequest)
}

/// Attempt to re-authenticate the session.
///
/// First validates the existing session; if invalid, performs full re-authentication.
/// If 2FA is required in headless mode, returns `AuthError::TwoFactorRequired`
/// so the caller can fire a notification and skip the current cycle.
///
/// # Lock strategy
///
/// A write lock is held across the `validate_session` call because validation
/// mutates the session (refreshes tokens). The lock is dropped before the
/// heavier `authenticate` call to avoid blocking download tasks. A 30-second
/// timeout guards against a hung validation request holding the lock
/// indefinitely.
pub(crate) async fn attempt_reauth(
    shared_session: &auth::SharedSession,
    cookie_directory: &Path,
    username: &str,
    domain: &str,
    password_provider: &crate::password::PasswordProvider,
    input_mode: crate::InputMode,
) -> anyhow::Result<()> {
    let mut session = shared_session.write().await;
    session.reacquire_lock()?;

    // Try validation first — timeout prevents a hung HTTP request from
    // holding the write lock indefinitely and starving download tasks.
    let valid = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        auth::validate_session(&mut session, domain),
    )
    .await
    .map_err(|_e| anyhow::anyhow!("Session validation timed out after 30s"))??;
    if valid {
        tracing::debug!("Session still valid after re-validation");
        return Ok(());
    }

    tracing::info!("Session invalid, performing full re-authentication...");
    let generation = session.generation();
    session.release_lock()?;
    drop(session);

    let new_auth = auth::authenticate_with_modes(
        cookie_directory,
        username,
        password_provider,
        domain,
        None,
        None,
        None, // no code — interactive prompt or TwoFactorRequired
        crate::personality::Mode::Off,
        input_mode,
        Some(generation),
    )
    .await?;

    let mut session = shared_session.write().await;
    *session = new_auth.session;
    tracing::info!("Re-authentication successful");
    Ok(())
}

/// Interval between polls when waiting for a 2FA code submission.
const TWO_FA_POLL_SECS: u64 = 5;

/// Maximum time to wait for `submit-code` to update persisted auth state.
const TWO_FA_WAIT_TIMEOUT_SECS: u64 = 10 * 60;

#[derive(Clone, Copy, Debug)]
struct TwoFaWaitConfig {
    poll_interval: Duration,
    timeout: Duration,
}

impl Default for TwoFaWaitConfig {
    fn default() -> Self {
        Self {
            poll_interval: Duration::from_secs(TWO_FA_POLL_SECS),
            timeout: Duration::from_secs(TWO_FA_WAIT_TIMEOUT_SECS),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AuthRetryErrorClass {
    TwoFactorRequired,
    LockContention,
    Other,
}

/// Classify auth retry outcomes at the service orchestration boundary.
///
/// The original `anyhow::Error` is still returned unchanged when callers need
/// to propagate it. This helper only centralizes the local retry/continue
/// decision so the 2FA wait loop and lock-contention retry loop cannot drift.
fn classify_auth_retry_error(err: &anyhow::Error) -> AuthRetryErrorClass {
    let Some(auth_err) = err.downcast_ref::<auth::error::AuthError>() else {
        return AuthRetryErrorClass::Other;
    };
    if auth_err.is_two_factor_required() {
        AuthRetryErrorClass::TwoFactorRequired
    } else if auth_err.is_lock_contention() {
        AuthRetryErrorClass::LockContention
    } else {
        AuthRetryErrorClass::Other
    }
}

#[derive(Debug, PartialEq, Eq)]
struct AuthSignalSnapshot {
    modified: Option<SystemTime>,
    contents: Option<Vec<u8>>,
}

fn auth_signal_paths(cookie_dir: &Path, username: &str) -> [std::path::PathBuf; 3] {
    [
        auth::session_file_path(cookie_dir, username),
        auth::cookiejar_file_path(cookie_dir, username),
        auth::validation_cache_file_path(cookie_dir, username),
    ]
}

async fn read_required_session_signal(session_path: &Path) -> anyhow::Result<AuthSignalSnapshot> {
    let contents = tokio::fs::read(session_path).await.map_err(|e| {
        let reason = match e.kind() {
            std::io::ErrorKind::NotFound => "missing",
            std::io::ErrorKind::PermissionDenied => "unreadable",
            _ => "unavailable",
        };
        anyhow::anyhow!(
            "Cannot wait for 2FA submission because session metadata file is {reason}: {path}. \
             Run `kei login get-code`, then `kei login submit-code <CODE>`, and restart the \
             service or rerun kei. Error: {e}",
            path = session_path.display()
        )
    })?;

    let modified = tokio::fs::metadata(session_path)
        .await
        .with_context(|| {
            format!(
                "Cannot wait for 2FA submission because session metadata file is unavailable: {}",
                session_path.display()
            )
        })?
        .modified()
        .with_context(|| {
            format!(
                "Cannot wait for 2FA submission because session metadata timestamp is unavailable: {}",
                session_path.display()
            )
        })?;

    Ok(AuthSignalSnapshot {
        modified: Some(modified),
        contents: Some(contents),
    })
}

async fn read_optional_auth_signal(path: &Path) -> AuthSignalSnapshot {
    let modified = tokio::fs::metadata(path)
        .await
        .and_then(|m| m.modified())
        .ok();
    let contents = tokio::fs::read(path).await.ok();
    AuthSignalSnapshot { modified, contents }
}

async fn read_auth_signal_snapshot(
    paths: &[std::path::PathBuf],
) -> anyhow::Result<Vec<AuthSignalSnapshot>> {
    let Some((session_path, optional_paths)) = paths.split_first() else {
        return Ok(Vec::new());
    };

    let mut snapshot = Vec::with_capacity(paths.len());
    snapshot.push(read_required_session_signal(session_path).await?);
    for path in optional_paths {
        snapshot.push(read_optional_auth_signal(path).await);
    }
    Ok(snapshot)
}

/// Wait for `submit-code` to update persisted auth state, with no network traffic.
///
/// Polls the session, cookie jar, and validation cache every 5 seconds. Most
/// Apple 2FA flows update session headers during trust, but the Extended Device
/// Protection 409 shape can accept the code without changing the session file.
/// `submit-code` still writes a fresh validation cache after accountLogin
/// succeeds, and that is enough to wake the service loop so it can retry auth.
async fn wait_for_2fa_submit_until(
    cookie_dir: &Path,
    username: &str,
    config: TwoFaWaitConfig,
    deadline: tokio::time::Instant,
) -> anyhow::Result<()> {
    let signal_paths = auth_signal_paths(cookie_dir, username);
    let initial = read_auth_signal_snapshot(&signal_paths).await?;
    let session_path = &signal_paths[0];

    tracing::info!(
        session_path = %session_path.display(),
        wait_mode = "auth_state_update",
        timeout_secs = config.timeout.as_secs(),
        poll_secs = config.poll_interval.as_secs(),
        "Waiting for 2FA code submission"
    );

    loop {
        let remaining = deadline
            .checked_duration_since(tokio::time::Instant::now())
            .unwrap_or_default();
        if remaining.is_zero() {
            tracing::warn!(
                session_path = %session_path.display(),
                wait_mode = "auth_state_update",
                timeout_secs = config.timeout.as_secs(),
                "Timed out waiting for 2FA code submission"
            );
            anyhow::bail!(
                "Timed out after {timeout_secs}s waiting for 2FA submission via persisted auth \
                 state under {path}. Run `kei login get-code`, then `kei login submit-code \
                 <CODE>`, and restart the service or rerun kei.",
                timeout_secs = config.timeout.as_secs(),
                path = cookie_dir.display()
            );
        }

        tokio::time::sleep(config.poll_interval.min(remaining)).await;

        let current = read_auth_signal_snapshot(&signal_paths).await?;
        if current != initial {
            tracing::debug!(
                session_path = %session_path.display(),
                wait_mode = "auth_state_update",
                "Auth state updated, retrying authentication"
            );
            return Ok(());
        }
    }
}

/// Wait for a 2FA code submission, then retry authentication with back-off.
///
/// Polls persisted auth state in a loop. After each auth-state change, retries
/// the provided `auth_fn` up to 3 times with 5-second back-off to handle lock
/// contention because `submit-code` may still be running when auth state
/// changes. False wakeups from get-code's SRP writes are handled by looping
/// back to wait when Apple still reports 2FA required.
pub(crate) async fn wait_and_retry_2fa<T, F, Fut>(
    cookie_dir: &Path,
    username: &str,
    auth_fn: F,
) -> anyhow::Result<T>
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<T>>,
{
    wait_and_retry_2fa_with_config(cookie_dir, username, auth_fn, TwoFaWaitConfig::default()).await
}

async fn wait_and_retry_2fa_with_config<T, F, Fut>(
    cookie_dir: &Path,
    username: &str,
    auth_fn: F,
    config: TwoFaWaitConfig,
) -> anyhow::Result<T>
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<T>>,
{
    let deadline = tokio::time::Instant::now() + config.timeout;

    loop {
        wait_for_2fa_submit_until(cookie_dir, username, config, deadline).await?;

        // Invalidate the validation cache so authenticate() actually checks
        // with Apple instead of returning stale cached data from before 2FA.
        let cache_path = auth::validation_cache_file_path(cookie_dir, username);
        if cache_path.exists()
            && let Err(e) = tokio::fs::remove_file(&cache_path).await
        {
            tracing::debug!(error = %e, "Could not remove validation cache");
        }

        for attempt in 0..3 {
            if attempt > 0 {
                tokio::time::sleep(std::time::Duration::from_secs(TWO_FA_POLL_SECS)).await;
            }
            match (auth_fn)().await {
                Ok(result) => return Ok(result),
                Err(e) => match classify_auth_retry_error(&e) {
                    AuthRetryErrorClass::TwoFactorRequired => {
                        tracing::debug!("Session not yet trusted, continuing to wait...");
                        break;
                    }
                    AuthRetryErrorClass::LockContention => {
                        tracing::debug!("Lock held by another process, retrying...");
                    }
                    AuthRetryErrorClass::Other => return Err(e),
                },
            }
        }
        tracing::debug!("Lock still held after retries, resuming wait...");
    }
}

/// Retry an auth operation on lock contention, with a brief wait.
///
/// Short-lived commands like `login get-code` and `login submit-code` may
/// collide with a `sync` process that is mid-auth (SRP takes a few seconds).
/// Instead of failing immediately, wait for the lock to be released.
pub(super) async fn retry_on_lock_contention<T, F, Fut>(auth_fn: F) -> anyhow::Result<T>
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<T>>,
{
    const MAX_ATTEMPTS: u32 = 6;
    const DELAY_SECS: u64 = 3;

    let mut last_err = None;
    for attempt in 0..MAX_ATTEMPTS {
        match (auth_fn)().await {
            Ok(result) => return Ok(result),
            Err(e) if classify_auth_retry_error(&e) == AuthRetryErrorClass::LockContention => {
                tracing::warn!(
                    attempt = attempt + 1,
                    max_attempts = MAX_ATTEMPTS,
                    "Another kei process is holding the session lock, retrying..."
                );
                last_err = Some(e);
                tokio::time::sleep(std::time::Duration::from_secs(DELAY_SECS)).await;
            }
            Err(e) => return Err(e),
        }
    }
    #[allow(
        clippy::expect_used,
        reason = "loop runs MAX_ATTEMPTS >= 1 times; last_err set on every iteration"
    )]
    Err(last_err.expect("MAX_ATTEMPTS must be >= 1"))
}

/// Build the query parameters `HashMap` for the iCloud Photos `CloudKit` API.
pub(crate) fn build_photos_params(
    client_id: &str,
    dsid: Option<&str>,
) -> std::collections::HashMap<String, serde_json::Value> {
    let mut params: std::collections::HashMap<String, serde_json::Value> =
        std::collections::HashMap::with_capacity(4);
    params.insert(
        "clientBuildNumber".into(),
        ICLOUD_CLIENT_BUILD_NUMBER.into(),
    );
    params.insert(
        "clientMasteringNumber".into(),
        ICLOUD_CLIENT_MASTERING_NUMBER.into(),
    );
    params.insert("clientId".into(), client_id.into());
    if let Some(dsid) = dsid {
        params.insert("dsid".into(), dsid.into());
    }
    params
}

/// Resolve a [`crate::selection::LibrarySelector`] into the concrete set of
/// `PhotoLibrary` instances the sync loop iterates over. Walks every zone the
/// account exposes (primary + private + shared), keeps the ones the
/// `primary` / `shared_all` flags or the `named` set include, drops any that
/// match `excluded`, and bails when a positive `named` entry resolves to
/// nothing.
///
/// `named` and `excluded` entries match a zone by exact zone name (case
/// insensitive) or by the truncated 8-char form
/// (`paths::truncate_library_zone`) so the on-disk `{library}` segment can
/// be copied straight back into `--library`.
pub(crate) async fn resolve_libraries(
    selector: &crate::selection::LibrarySelector,
    photos_service: &mut icloud::photos::PhotosService,
) -> anyhow::Result<Vec<icloud::photos::PhotoLibrary>> {
    use crate::download::paths::truncate_library_zone;

    // Fast path for the default `--library primary`: skip the private +
    // shared library HTTP listings. Saves two requests per sync for the
    // common single-library case.
    if selector == &crate::selection::LibrarySelector::default() {
        let lib = photos_service
            .get_library(crate::icloud::photos::PRIMARY_ZONE_NAME)
            .await?;
        return Ok(vec![lib.clone()]);
    }

    let all = photos_service.all_libraries().await?;

    // Track which selector entries actually matched so we can bail on a
    // positive miss (the spec's "Album 'Vacatiom' not found" rule, applied to
    // libraries). `named` only; `excluded` misses just warn.
    let mut named_hits: std::collections::HashSet<&str> =
        std::collections::HashSet::with_capacity(selector.named.len());
    let mut chosen: Vec<icloud::photos::PhotoLibrary> = Vec::new();

    for lib in &all {
        let zone = lib.zone_name();
        let truncated = truncate_library_zone(zone);
        let is_primary = zone == crate::icloud::photos::PRIMARY_ZONE_NAME;
        let is_shared = crate::icloud::photos::is_shared_zone(zone);

        let included = (selector.primary && is_primary)
            || (selector.shared_all && is_shared)
            || selector.named.iter().any(|entry| {
                let hit = library_entry_matches_zone(entry, zone, truncated);
                if hit {
                    named_hits.insert(entry.as_str());
                }
                hit
            });
        if !included {
            continue;
        }

        let excluded = selector.excluded.iter().any(|entry| {
            (entry.eq_ignore_ascii_case("primary") && is_primary)
                || (entry.eq_ignore_ascii_case("shared") && is_shared)
                || library_entry_matches_zone(entry, zone, truncated)
        });
        if excluded {
            continue;
        }

        // `all_libraries()` returns disjoint sets (primary, private minus
        // PrimarySync, shared) so no cross-source dedup is needed here.
        chosen.push(lib.clone());
    }

    if let Some(missed) = selector
        .named
        .iter()
        .find(|n| !named_hits.contains(n.as_str()))
    {
        let known: Vec<&str> = all
            .iter()
            .map(icloud::photos::PhotoLibrary::zone_name)
            .collect();
        anyhow::bail!(
            "`--library {missed}` did not match any iCloud Photos library. Available zones: {}. Run `kei list libraries` to see every zone with its truncated form.",
            known.join(", ")
        );
    }

    if chosen.is_empty() {
        anyhow::bail!(
            "`--library` did not select any libraries for this account. Choose `primary`, `shared`, or a zone name, and do not exclude everything."
        );
    }

    // Path collision guard. `truncate_library_zone` keeps the first 8 hex
    // chars of `SharedSync-<UUID>`; CloudKit UUIDs are not guaranteed
    // unique on that prefix, so two distinct shared zones can render to
    // the same `{library}` path segment. If both land in the chosen set,
    // multi-library sync silently overwrites one zone's bytes with the
    // other's (state DB stays per-zone distinct under the v8 PK so the
    // corruption is invisible). Bail with both full UUIDs and the
    // truncated form so the user can pin a longer prefix via `--library
    // SharedSync-<longer>`.
    let mut by_truncated: std::collections::HashMap<&str, Vec<&str>> =
        std::collections::HashMap::with_capacity(chosen.len());
    for lib in &chosen {
        let zone = lib.zone_name();
        by_truncated
            .entry(truncate_library_zone(zone))
            .or_default()
            .push(zone);
    }
    if let Some((truncated, zones)) = by_truncated.iter().find(|(_, zones)| zones.len() > 1) {
        let mut sorted = zones.clone();
        sorted.sort_unstable();
        anyhow::bail!(
            "Multiple selected libraries would use the same `{{library}}` folder name `{truncated}`: [{full}]. Select one explicitly with a longer `--library <prefix>` value. Run `kei list libraries` to see the full zone names.",
            full = sorted.join(", "),
        );
    }

    match chosen.as_slice() {
        [only] if only.zone_name() != crate::icloud::photos::PRIMARY_ZONE_NAME => {
            tracing::debug!(library = %only.zone_name(), "Using non-default library");
        }
        [_] => {} // primary-only logged in the fast path above
        many => tracing::debug!(count = many.len(), "Using multiple libraries"),
    }

    Ok(chosen)
}

pub(crate) async fn resolve_cross_zone_libraries_for_album_hydration<Fut>(
    selection: &crate::selection::Selection,
    all_libraries: Fut,
) -> anyhow::Result<Vec<icloud::photos::PhotoLibrary>>
where
    Fut: std::future::Future<Output = anyhow::Result<Vec<icloud::photos::PhotoLibrary>>>,
{
    use crate::selection::AlbumSelector;
    use anyhow::Context;

    if matches!(selection.albums, AlbumSelector::None) || !selection.albums_explicit {
        return Ok(Vec::new());
    }

    all_libraries
        .await
        .context("Could not resolve libraries needed for cross-library album matching")
}

/// Match a `--library` entry (full zone name or truncated 8-char form)
/// against a live zone, case-insensitive. The truncated form is what
/// `{library}` renders into paths, so users can copy a path segment and
/// paste it back into `--library`.
fn library_entry_matches_zone(entry: &str, zone: &str, truncated: &str) -> bool {
    entry.eq_ignore_ascii_case(zone) || entry.eq_ignore_ascii_case(truncated)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PassScope {
    pub include_albums: bool,
    pub include_smart_folders: bool,
    pub include_unfiled: bool,
}

impl PassScope {
    pub(crate) fn is_empty(self) -> bool {
        !self.include_albums && !self.include_smart_folders && !self.include_unfiled
    }
}

#[derive(Debug, Clone)]
pub(crate) struct CollectionContext {
    pub(crate) collection_album_names: std::collections::BTreeSet<String>,
    pub(crate) selected_smart_folder_names: Vec<String>,
}

pub(crate) fn smart_selector_active(selection: &crate::selection::Selection) -> bool {
    !matches!(
        selection.smart_folders,
        crate::selection::SmartFolderSelector::None
    )
}

pub(crate) fn collection_libraries<'a>(
    selection: &crate::selection::Selection,
    selected_libraries: &'a [icloud::photos::PhotoLibrary],
    all_libraries: &'a [icloud::photos::PhotoLibrary],
) -> &'a [icloud::photos::PhotoLibrary] {
    // Explicit collection selectors widen album/smart-folder enumeration to
    // every visible library. Unfiled remains selected-library scoped via
    // `pass_scope_for_zone`.
    if selection.albums_explicit || smart_selector_active(selection) {
        all_libraries
    } else {
        selected_libraries
    }
}

pub(crate) fn zone_name_set(
    libraries: &[icloud::photos::PhotoLibrary],
) -> rustc_hash::FxHashSet<String> {
    libraries
        .iter()
        .map(|library| library.zone_name().to_string())
        .collect()
}

pub(crate) fn pass_scope_for_zone(
    selection: &crate::selection::Selection,
    zone_name: &str,
    selected_zones: &rustc_hash::FxHashSet<String>,
    collection_zones: &rustc_hash::FxHashSet<String>,
) -> PassScope {
    use crate::selection::AlbumSelector;

    let include_unfiled = selection.unfiled && selected_zones.contains(zone_name);
    let include_albums = match selection.albums {
        AlbumSelector::None => false,
        _ if selection.albums_explicit => collection_zones.contains(zone_name),
        _ => selected_zones.contains(zone_name),
    };
    let include_smart_folders =
        smart_selector_active(selection) && collection_zones.contains(zone_name);
    PassScope {
        include_albums,
        include_smart_folders,
        include_unfiled,
    }
}

pub(crate) async fn build_collection_context(
    selection: &crate::selection::Selection,
    collection_libraries: &[icloud::photos::PhotoLibrary],
) -> anyhow::Result<CollectionContext> {
    use crate::selection::AlbumSelector;

    let smart_names = smart_folder_name_set();
    let selected_smart_folder_names =
        pick_selected_smart_folder_names(&selection.smart_folders, &smart_names)?;

    let mut collection_album_names = std::collections::BTreeSet::new();
    if selection.albums_explicit && !matches!(selection.albums, AlbumSelector::None) {
        for library in collection_libraries {
            let album_map = library.albums().await?;
            for name in album_map.keys() {
                if !smart_names.contains(name.as_str()) {
                    collection_album_names.insert(name.clone());
                }
            }
        }
        validate_collection_album_selector(
            &selection.albums,
            &collection_album_names,
            &smart_names,
        )?;
    }

    Ok(CollectionContext {
        collection_album_names,
        selected_smart_folder_names,
    })
}

fn validate_collection_album_selector(
    selector: &crate::selection::AlbumSelector,
    collection_album_names: &std::collections::BTreeSet<String>,
    smart_names: &rustc_hash::FxHashSet<&'static str>,
) -> anyhow::Result<()> {
    use crate::selection::AlbumSelector;
    match selector {
        AlbumSelector::None => Ok(()),
        AlbumSelector::All { excluded } => {
            bail_unknown_excluded_collection_albums(excluded, collection_album_names)
        }
        AlbumSelector::Named { included, excluded } => {
            for name in included {
                if smart_names.contains(name.as_str()) {
                    anyhow::bail!(
                        "`{name}` is a smart folder. Use `--smart-folder {name}` instead of `--album {name}`."
                    );
                }
                if excluded.contains(name) {
                    continue;
                }
                if !collection_album_names.contains(name) {
                    let available: Vec<&str> =
                        collection_album_names.iter().map(String::as_str).collect();
                    anyhow::bail!("Album `{name}` was not found. Available albums: {available:?}");
                }
            }
            bail_unknown_excluded_collection_albums(excluded, collection_album_names)
        }
    }
}

fn bail_unknown_excluded_collection_albums(
    excluded: &std::collections::BTreeSet<String>,
    collection_album_names: &std::collections::BTreeSet<String>,
) -> anyhow::Result<()> {
    for name in excluded {
        if !collection_album_names.contains(name) {
            let available: Vec<&str> = collection_album_names.iter().map(String::as_str).collect();
            anyhow::bail!("Excluded album `{name}` was not found. Available albums: {available:?}");
        }
    }
    Ok(())
}

fn pick_selected_smart_folder_names(
    selector: &crate::selection::SmartFolderSelector,
    smart_names: &rustc_hash::FxHashSet<&'static str>,
) -> anyhow::Result<Vec<String>> {
    use crate::selection::SmartFolderSelector;
    let sensitive: rustc_hash::FxHashSet<&'static str> =
        icloud::photos::smart_folders::sensitive_smart_folder_names().collect();

    match selector {
        SmartFolderSelector::None => Ok(Vec::new()),
        SmartFolderSelector::All {
            include_sensitive,
            excluded,
        } => {
            for name in excluded {
                bail_excluded_not_a_smart_folder(name, smart_names)?;
            }
            let mut names: Vec<String> = smart_names
                .iter()
                .filter(|name| *include_sensitive || !sensitive.contains(**name))
                .filter(|name| !excluded.contains(**name))
                .map(|name| (*name).to_string())
                .collect();
            names.sort();
            Ok(names)
        }
        SmartFolderSelector::Named { included, excluded } => {
            for name in included {
                if !smart_names.contains(name.as_str()) {
                    let mut available: Vec<&str> = smart_names.iter().copied().collect();
                    available.sort();
                    anyhow::bail!(
                        "`{name}` is not an Apple smart folder. Available smart folders: {available:?}"
                    );
                }
            }
            for name in excluded {
                bail_excluded_not_a_smart_folder(name, smart_names)?;
            }
            Ok(included
                .iter()
                .filter(|name| !excluded.contains(*name))
                .cloned()
                .collect())
        }
    }
}

/// Category of a download pass: a named user album, an Apple-defined smart
/// folder, or the library-wide unfiled pseudo-pass. Drives template/token
/// selection in the path renderer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PassKind {
    Album,
    SmartFolder,
    Unfiled,
}

impl PassKind {
    /// Folder-structure token expanded for this pass kind. The unfiled pass
    /// reuses `{album}` so existing configs with `--folder-structure
    /// "{album}/..."` still produce the same on-disk tree (the token
    /// collapses to an empty segment when the unfiled pass runs with the
    /// library-wide pseudo-album's empty name).
    pub(crate) fn token(self) -> &'static str {
        match self {
            Self::Album | Self::Unfiled => "{album}",
            Self::SmartFolder => "{smart-folder}",
        }
    }
}

/// One pass through a specific album (or the library-wide pseudo-album).
///
/// `exclude_ids` is the per-pass set of asset IDs to filter out. Most passes
/// carry an empty set. Full sync resolves the library-wide unfiled pass's
/// album-member exclusions inside the download phase so album and unfiled
/// enumeration can overlap; incremental and cleanup paths resolve those
/// exclusions before planning tasks.
#[derive(Clone)]
pub(crate) struct AlbumPass {
    pub kind: PassKind,
    pub album: icloud::photos::PhotoAlbum,
    pub exclude_ids: std::sync::Arc<rustc_hash::FxHashSet<String>>,
}

/// Ordered list of download passes for a single library.
#[derive(Debug)]
pub(crate) struct AlbumPlan {
    pub passes: Vec<AlbumPass>,
}

impl std::fmt::Debug for AlbumPass {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AlbumPass")
            .field("kind", &self.kind)
            .field("album_name", &self.album.name)
            .field("exclude_ids_len", &self.exclude_ids.len())
            .finish()
    }
}

fn empty_exclude_ids() -> std::sync::Arc<rustc_hash::FxHashSet<String>> {
    std::sync::Arc::new(rustc_hash::FxHashSet::default())
}

/// Enumerate every asset ID in an album into `into`. Kept for resolver tests
/// that pin the old preflight error behavior; production resolves unfiled
/// exclusions in the download phase.
#[cfg(test)]
async fn collect_album_asset_ids(
    album: &icloud::photos::PhotoAlbum,
    into: &mut rustc_hash::FxHashSet<String>,
) -> anyhow::Result<()> {
    use anyhow::Context;
    use futures_util::StreamExt;
    // Propagate `len()` failures — the count is load-bearing for the
    // `-a all` + `{album}` unfiled pass: a silent 0 here leaves the
    // exclusion set incomplete and the unfiled pass re-downloads assets
    // that are already in some user album.
    let count = album
        .len()
        .await
        .with_context(|| format!("Could not count assets in album `{}`", album.name))?;
    let (stream, _token_rx) = album.photo_stream_with_token(None, Some(count), 1);
    tokio::pin!(stream);
    while let Some(result) = stream.next().await {
        let asset = result?;
        into.insert(asset.asset_record_name().to_string());
    }
    Ok(())
}

/// Static set of Apple-defined smart-folder names. Looked up by name on
/// the same map `PhotoLibrary::albums` returns; used to split user albums
/// vs smart folders for the v0.13 selection model.
fn smart_folder_name_set() -> rustc_hash::FxHashSet<&'static str> {
    icloud::photos::smart_folders::smart_folder_names().collect()
}

/// Resolve the v0.13 [`crate::selection::Selection`] into concrete download
/// passes for one library.
///
/// Per-category behaviour, mirroring the spec:
///
/// - `albums: None` → no album passes.
/// - `albums: All { excluded }` → one pass per user album in the library
///   except those listed in `excluded`. Missing exclusion names log a
///   warning and are skipped; smart folders are filtered out (use
///   `--smart-folder all` to opt in).
/// - `albums: Named { included, excluded }` → one pass per name in
///   `included`. Missing names bail at startup with the available album
///   list. Names that are smart folders bail with a hint to use
///   `--smart-folder` instead.
/// - `smart_folders: None` → no smart-folder passes.
/// - `smart_folders: All { include_sensitive, excluded }` → one pass per
///   smart folder; Hidden / Recently Deleted are skipped unless
///   `include_sensitive` is true. Missing exclusion names log a warning.
/// - `smart_folders: Named { included, excluded }` → one pass per smart
///   folder name; non-smart-folder names bail.
/// - `unfiled: true` → an extra library-wide pass with `exclude_ids`
///   covering every member of every selected album (the spec dedup
///   invariant: an asset in a selected album must not also land at the
///   unfiled path). Selected smart folders do not contribute to the
///   exclusion set — smart-folder membership is orthogonal to album
///   membership.
///
/// Album member IDs are fetched in parallel before the album map is
/// consumed into passes; each `PhotoAlbum` is moved into exactly one pass.
#[cfg(test)]
pub(crate) async fn resolve_passes(
    library: &icloud::photos::PhotoLibrary,
    selection: &crate::selection::Selection,
    cross_zone_libraries: &[icloud::photos::PhotoLibrary],
) -> anyhow::Result<AlbumPlan> {
    let mut selection_for_test = selection.clone();
    selection_for_test.albums_explicit = false;
    let scope = PassScope {
        include_albums: true,
        include_smart_folders: true,
        include_unfiled: selection_for_test.unfiled,
    };
    let collection_context = build_collection_context(&selection_for_test, &[]).await?;
    resolve_passes_for_scope(
        library,
        &selection_for_test,
        scope,
        &collection_context,
        cross_zone_libraries,
    )
    .await
}

pub(crate) async fn resolve_passes_for_scope(
    library: &icloud::photos::PhotoLibrary,
    selection: &crate::selection::Selection,
    scope: PassScope,
    collection_context: &CollectionContext,
    cross_zone_libraries: &[icloud::photos::PhotoLibrary],
) -> anyhow::Result<AlbumPlan> {
    use crate::selection::{AlbumSelector, SmartFolderSelector};

    let album_active = scope.include_albums && !matches!(selection.albums, AlbumSelector::None);
    let smart_active = scope.include_smart_folders
        && !matches!(selection.smart_folders, SmartFolderSelector::None);
    let unfiled_active = scope.include_unfiled;

    if !album_active && !smart_active && !unfiled_active {
        return Ok(AlbumPlan { passes: Vec::new() });
    }

    let mut album_map = if album_active || smart_active {
        library.albums().await?
    } else {
        std::collections::HashMap::new()
    };
    let smart_names = smart_folder_name_set();

    let selected_album_names = if album_active {
        if selection.albums_explicit {
            pick_collection_scoped_album_names_for_library(
                &selection.albums,
                &album_map,
                &smart_names,
                &collection_context.collection_album_names,
            )?
        } else {
            pick_album_names(&selection.albums, &album_map, &smart_names)?
        }
    } else {
        Vec::new()
    };
    let selected_smart_names = if smart_active {
        pick_collection_scoped_smart_folder_names_for_library(
            library,
            &album_map,
            &collection_context.selected_smart_folder_names,
        )
    } else {
        Vec::new()
    };

    let empty = empty_exclude_ids();
    let mut passes: Vec<AlbumPass> = Vec::new();
    let cross_zone_album_sources: Vec<icloud::photos::PhotoAlbum> = cross_zone_libraries
        .iter()
        .filter(|source| source.zone_name() != library.zone_name())
        .map(icloud::photos::PhotoLibrary::all)
        .collect();

    // Stable, alphabetised pass order so logs and dry-run output don't
    // jitter with HashMap iteration order.
    drain_named_into_passes(
        &mut album_map,
        selected_album_names,
        PassKind::Album,
        &empty,
        &cross_zone_album_sources,
        &mut passes,
    );
    drain_named_into_passes(
        &mut album_map,
        selected_smart_names,
        PassKind::SmartFolder,
        &empty,
        &[],
        &mut passes,
    );

    if unfiled_active {
        passes.push(AlbumPass {
            kind: PassKind::Unfiled,
            album: library.all(),
            exclude_ids: empty_exclude_ids(),
        });
    }

    Ok(AlbumPlan { passes })
}

fn pick_collection_scoped_album_names_for_library(
    selector: &crate::selection::AlbumSelector,
    album_map: &std::collections::HashMap<String, icloud::photos::PhotoAlbum>,
    smart_names: &rustc_hash::FxHashSet<&'static str>,
    collection_album_names: &std::collections::BTreeSet<String>,
) -> anyhow::Result<Vec<String>> {
    use crate::selection::AlbumSelector;

    match selector {
        AlbumSelector::None => Ok(Vec::new()),
        AlbumSelector::All { excluded } => {
            bail_unknown_excluded_collection_albums(excluded, collection_album_names)?;
            Ok(album_map
                .keys()
                .filter(|name| {
                    !smart_names.contains(name.as_str()) && !excluded.contains(name.as_str())
                })
                .cloned()
                .collect())
        }
        AlbumSelector::Named { included, excluded } => {
            validate_collection_album_selector(selector, collection_album_names, smart_names)?;
            Ok(included
                .iter()
                .filter(|name| !excluded.contains(*name))
                .filter(|name| album_map.contains_key(name.as_str()))
                .cloned()
                .collect())
        }
    }
}

fn pick_collection_scoped_smart_folder_names_for_library(
    library: &icloud::photos::PhotoLibrary,
    album_map: &std::collections::HashMap<String, icloud::photos::PhotoAlbum>,
    selected_smart_folder_names: &[String],
) -> Vec<String> {
    selected_smart_folder_names
        .iter()
        .filter_map(|name| {
            if album_map.contains_key(name.as_str()) {
                Some(name.clone())
            } else {
                tracing::warn!(
                    zone = %library.zone_name(),
                    smart_folder = %name,
                    "Smart folder not present in this library, skipping"
                );
                None
            }
        })
        .collect()
}

/// Pick the user-album names selected by `albums`. Bails on missing
/// `Named` includes and on missing `excluded` entries (a typo'd exclusion
/// would silently match nothing; better to fail loudly so the user can
/// correct it).
fn pick_album_names(
    selector: &crate::selection::AlbumSelector,
    album_map: &std::collections::HashMap<String, icloud::photos::PhotoAlbum>,
    smart_names: &rustc_hash::FxHashSet<&'static str>,
) -> anyhow::Result<Vec<String>> {
    use crate::selection::AlbumSelector;
    match selector {
        AlbumSelector::None => Ok(Vec::new()),
        AlbumSelector::All { excluded } => {
            bail_unknown_excluded_albums(excluded, album_map, smart_names)?;
            Ok(album_map
                .keys()
                .filter(|name| {
                    !smart_names.contains(name.as_str()) && !excluded.contains(name.as_str())
                })
                .cloned()
                .collect())
        }
        AlbumSelector::Named { included, excluded } => {
            let mut chosen = Vec::with_capacity(included.len());
            for name in included {
                if smart_names.contains(name.as_str()) {
                    anyhow::bail!(
                        "`{name}` is a smart folder. Use `--smart-folder {name}` instead of `--album {name}`."
                    );
                }
                if excluded.contains(name) {
                    continue;
                }
                if album_map.contains_key(name.as_str()) {
                    chosen.push(name.clone());
                } else {
                    let mut available: Vec<&String> = album_map
                        .keys()
                        .filter(|k| !smart_names.contains(k.as_str()))
                        .collect();
                    available.sort();
                    anyhow::bail!("Album `{name}` was not found. Available albums: {available:?}");
                }
            }
            bail_unknown_excluded_albums(excluded, album_map, smart_names)?;
            Ok(chosen)
        }
    }
}

/// Bail on excluded album names that don't exist. A typo in `--album '!Family'`
/// would silently match nothing under the otherwise-default `all` selector;
/// surfacing it as a hard error keeps the user's intent honest.
fn bail_unknown_excluded_albums(
    excluded: &std::collections::BTreeSet<String>,
    album_map: &std::collections::HashMap<String, icloud::photos::PhotoAlbum>,
    smart_names: &rustc_hash::FxHashSet<&'static str>,
) -> anyhow::Result<()> {
    for name in excluded {
        if !album_map.contains_key(name.as_str()) {
            let mut available: Vec<&String> = album_map
                .keys()
                .filter(|k| !smart_names.contains(k.as_str()))
                .collect();
            available.sort();
            anyhow::bail!("Excluded album `{name}` was not found. Available albums: {available:?}");
        }
    }
    Ok(())
}

fn bail_excluded_not_a_smart_folder(
    name: &str,
    smart_names: &rustc_hash::FxHashSet<&'static str>,
) -> anyhow::Result<()> {
    if !smart_names.contains(name) {
        let mut available: Vec<&str> = smart_names.iter().copied().collect();
        available.sort();
        anyhow::bail!(
            "Excluded smart folder `{name}` was not found. Available smart folders: {available:?}"
        );
    }
    Ok(())
}

/// Fetch every selected album's member IDs in parallel. Test-only coverage
/// for missing-album handling retained from the old preflight resolver.
#[cfg(test)]
async fn compute_unfiled_exclude_ids(
    album_map: &std::collections::HashMap<String, icloud::photos::PhotoAlbum>,
    selected_album_names: &[String],
) -> anyhow::Result<rustc_hash::FxHashSet<String>> {
    use futures_util::{StreamExt, TryStreamExt};
    const EXCLUSION_FETCH_CONCURRENCY: usize = 8;

    if selected_album_names.is_empty() {
        return Ok(rustc_hash::FxHashSet::default());
    }

    // Defensive: pick_album_names validated every name against album_map,
    // so a miss here means the map mutated between checks. Bail loudly
    // rather than silently shrinking the exclusion set, which would let
    // already-filed assets re-download under --unfiled.
    let mut tuples: Vec<(&String, &icloud::photos::PhotoAlbum)> =
        Vec::with_capacity(selected_album_names.len());
    for name in selected_album_names {
        match album_map.get(name.as_str()) {
            Some(album) => tuples.push((name, album)),
            None => anyhow::bail!(
                "Selected album '{name}' missing from library album map at \
                 unfiled-exclusion time"
            ),
        }
    }

    let per_album: Vec<rustc_hash::FxHashSet<String>> = futures_util::stream::iter(tuples)
        .map(|(name, album)| async move {
            tracing::debug!(album = %name, "Pre-fetching IDs for unfiled exclusion set");
            let mut set = rustc_hash::FxHashSet::default();
            collect_album_asset_ids(album, &mut set).await?;
            anyhow::Ok(set)
        })
        .buffer_unordered(EXCLUSION_FETCH_CONCURRENCY)
        .try_collect()
        .await?;

    let mut union = rustc_hash::FxHashSet::default();
    for set in per_album {
        union.extend(set);
    }
    Ok(union)
}

/// Move named entries out of `album_map` into `passes` in alphabetical
/// order, sharing `empty_excludes` as the per-pass exclude set. A `None`
/// from `remove` should be impossible (`pick_*` validates membership) but
/// is logged + skipped to keep the resolver out of unwrap territory.
fn drain_named_into_passes(
    album_map: &mut std::collections::HashMap<String, icloud::photos::PhotoAlbum>,
    mut names: Vec<String>,
    kind: PassKind,
    empty_excludes: &std::sync::Arc<rustc_hash::FxHashSet<String>>,
    cross_zone_sources: &[icloud::photos::PhotoAlbum],
    passes: &mut Vec<AlbumPass>,
) {
    names.sort();
    for name in names {
        let Some(album) = album_map.remove(name.as_str()) else {
            tracing::warn!(category = ?kind, name = %name, "Selected entry disappeared from map, skipping");
            continue;
        };
        let album = if kind == PassKind::Album && !cross_zone_sources.is_empty() {
            let sources = cross_zone_sources
                .iter()
                .map(|source| source.clone_for_cross_zone_source())
                .collect();
            album.with_cross_zone_sources(sources)
        } else {
            album
        };
        passes.push(AlbumPass {
            kind,
            album,
            exclude_ids: std::sync::Arc::clone(empty_excludes),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── build_photos_params tests ───────────────────────────────────────

    #[test]
    fn build_photos_params_includes_client_id_and_dsid() {
        let params = build_photos_params("test-client-id-123", Some("99999"));

        assert_eq!(
            params.get("clientBuildNumber"),
            Some(&serde_json::Value::String(
                ICLOUD_CLIENT_BUILD_NUMBER.to_string()
            ))
        );
        assert_eq!(
            params.get("clientMasteringNumber"),
            Some(&serde_json::Value::String(
                ICLOUD_CLIENT_MASTERING_NUMBER.to_string()
            ))
        );
        assert_eq!(
            params.get("clientId"),
            Some(&serde_json::Value::String("test-client-id-123".to_string()))
        );
        assert_eq!(
            params.get("dsid"),
            Some(&serde_json::Value::String("99999".to_string()))
        );
    }

    #[test]
    fn build_photos_params_no_dsid() {
        let params = build_photos_params("client-abc", None);

        assert!(!params.contains_key("dsid"));
        assert_eq!(
            params.get("clientId"),
            Some(&serde_json::Value::String("client-abc".to_string()))
        );
    }

    #[test]
    fn build_photos_params_empty_client_id() {
        let params = build_photos_params("", Some("12345"));

        assert_eq!(
            params.get("clientId"),
            Some(&serde_json::Value::String(String::new()))
        );
        assert_eq!(
            params.get("dsid"),
            Some(&serde_json::Value::String("12345".to_string()))
        );
    }

    // ── resolve_passes tests ─────────────────────────────────────────

    use crate::icloud::photos::PhotoLibrary;
    use crate::selection::{AlbumSelector, Selection, SmartFolderSelector};
    use crate::test_helpers::MockPhotosSession;
    use std::collections::BTreeSet;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn short_2fa_wait_config(timeout: Duration) -> TwoFaWaitConfig {
        TwoFaWaitConfig {
            poll_interval: Duration::from_millis(10),
            timeout,
        }
    }

    fn classify_auth_retry_error_for(err: auth::error::AuthError) -> AuthRetryErrorClass {
        let err = anyhow::Error::new(err);
        classify_auth_retry_error(&err)
    }

    // ── auth retry classifier tests ──────────────────────────────────

    #[test]
    fn classify_auth_retry_error_detects_two_factor_required() {
        assert_eq!(
            classify_auth_retry_error_for(auth::error::AuthError::TwoFactorRequired),
            AuthRetryErrorClass::TwoFactorRequired
        );
    }

    #[test]
    fn classify_auth_retry_error_detects_lock_contention() {
        assert_eq!(
            classify_auth_retry_error_for(auth::error::AuthError::LockContention(
                "session.lock".into()
            )),
            AuthRetryErrorClass::LockContention
        );
    }

    #[test]
    fn classify_auth_retry_error_treats_failed_login_as_other() {
        assert_eq!(
            classify_auth_retry_error_for(auth::error::AuthError::FailedLogin(
                "bad credentials".into()
            )),
            AuthRetryErrorClass::Other
        );
    }

    #[test]
    fn classify_auth_retry_error_treats_plain_anyhow_as_other() {
        let err = anyhow::anyhow!("plain failure");
        assert_eq!(classify_auth_retry_error(&err), AuthRetryErrorClass::Other);
    }

    #[test]
    fn classify_auth_retry_error_detects_context_wrapped_auth_errors() {
        let two_factor =
            anyhow::Error::new(auth::error::AuthError::TwoFactorRequired).context("service retry");
        assert_eq!(
            classify_auth_retry_error(&two_factor),
            AuthRetryErrorClass::TwoFactorRequired
        );

        let lock = anyhow::Error::new(auth::error::AuthError::LockContention(
            "session.lock".into(),
        ))
        .context("service retry");
        assert_eq!(
            classify_auth_retry_error(&lock),
            AuthRetryErrorClass::LockContention
        );
    }

    // ── service 2FA wait tests ────────────────────────────────────────

    #[tokio::test]
    async fn wait_and_retry_2fa_succeeds_after_session_metadata_update() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let cookie_dir = tempdir.path().to_path_buf();
        let username = "service-wait@example.com";
        let session_path = auth::session_file_path(&cookie_dir, username);
        tokio::fs::write(&session_path, br#"{"session_token":"before"}"#)
            .await
            .expect("write initial session metadata");

        let auth_calls = Arc::new(AtomicUsize::new(0));
        let task = tokio::spawn({
            let cookie_dir = cookie_dir.clone();
            let auth_calls = Arc::clone(&auth_calls);
            async move {
                wait_and_retry_2fa_with_config(
                    &cookie_dir,
                    username,
                    || {
                        let auth_calls = Arc::clone(&auth_calls);
                        async move {
                            auth_calls.fetch_add(1, Ordering::SeqCst);
                            Ok::<_, anyhow::Error>("authenticated")
                        }
                    },
                    short_2fa_wait_config(Duration::from_secs(3)),
                )
                .await
            }
        });

        tokio::time::sleep(Duration::from_millis(50)).await;
        tokio::fs::write(&session_path, br#"{"session_token":"after"}"#)
            .await
            .expect("update session metadata");

        let result = tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .expect("2FA wait should finish")
            .expect("task should not panic")
            .expect("updated metadata should retry auth");

        assert_eq!(result, "authenticated");
        assert_eq!(auth_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn wait_for_2fa_submit_wakes_when_validation_cache_changes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let username = "user@example.com";
        let cache_path = auth::validation_cache_file_path(dir.path(), username);

        assert_wait_for_2fa_submit_wakes_after_write(
            dir.path(),
            username,
            cache_path,
            b"{\"validated_at\":1}",
            "validation cache write",
        )
        .await;
    }

    #[tokio::test]
    async fn wait_for_2fa_submit_wakes_when_cookiejar_changes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let username = "user@example.com";
        let cookiejar_path = auth::cookiejar_file_path(dir.path(), username);

        assert_wait_for_2fa_submit_wakes_after_write(
            dir.path(),
            username,
            cookiejar_path,
            b"[]",
            "cookie jar write",
        )
        .await;
    }

    async fn assert_wait_for_2fa_submit_wakes_after_write(
        cookie_dir: &Path,
        username: &str,
        signal_path: PathBuf,
        contents: &'static [u8],
        context: &'static str,
    ) {
        let session_path = auth::session_file_path(cookie_dir, username);
        tokio::fs::write(&session_path, br#"{"session_token":"before"}"#)
            .await
            .expect("write initial session metadata");

        tokio::time::timeout(Duration::from_secs(1), async {
            tokio::join!(
                wait_for_2fa_submit_until(
                    cookie_dir,
                    username,
                    short_2fa_wait_config(Duration::from_secs(1)),
                    tokio::time::Instant::now() + Duration::from_secs(1),
                ),
                async {
                    tokio::time::sleep(Duration::from_millis(30)).await;
                    tokio::fs::write(signal_path, contents)
                        .await
                        .expect(context);
                }
            )
            .0
            .expect("auth state write should wake the 2FA waiter");
        })
        .await
        .expect("auth state write should wake before timeout");
    }

    #[tokio::test]
    async fn wait_and_retry_2fa_fails_when_session_metadata_missing() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let auth_calls = Arc::new(AtomicUsize::new(0));

        let err = wait_and_retry_2fa_with_config(
            tempdir.path(),
            "missing-metadata@example.com",
            || {
                let auth_calls = Arc::clone(&auth_calls);
                async move {
                    auth_calls.fetch_add(1, Ordering::SeqCst);
                    Ok::<_, anyhow::Error>(())
                }
            },
            short_2fa_wait_config(Duration::from_secs(1)),
        )
        .await
        .expect_err("missing metadata must fail before waiting forever");

        let msg = err.to_string();
        assert!(
            msg.contains("session metadata file is missing"),
            "msg: {msg}"
        );
        assert!(
            msg.contains("kei login get-code") && msg.contains("kei login submit-code <CODE>"),
            "msg: {msg}"
        );
        assert_eq!(auth_calls.load(Ordering::SeqCst), 0);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn wait_and_retry_2fa_fails_when_session_metadata_unreadable() {
        use std::os::unix::fs::PermissionsExt;

        let tempdir = tempfile::tempdir().expect("tempdir");
        let username = "unreadable-metadata@example.com";
        let session_path = auth::session_file_path(tempdir.path(), username);
        tokio::fs::write(&session_path, br#"{"session_token":"before"}"#)
            .await
            .expect("write initial session metadata");
        tokio::fs::set_permissions(&session_path, std::fs::Permissions::from_mode(0o000))
            .await
            .expect("make session metadata unreadable");

        let err = wait_and_retry_2fa_with_config(
            tempdir.path(),
            username,
            || async { Ok::<_, anyhow::Error>(()) },
            short_2fa_wait_config(Duration::from_millis(30)),
        )
        .await
        .expect_err("unreadable metadata must fail instead of waiting forever");

        tokio::fs::set_permissions(&session_path, std::fs::Permissions::from_mode(0o600))
            .await
            .expect("restore permissions for tempdir cleanup");

        let msg = err.to_string();
        assert!(
            msg.contains("session metadata file is unreadable"),
            "msg: {msg}"
        );
        assert!(
            msg.contains(&session_path.display().to_string()),
            "msg: {msg}"
        );
    }

    #[tokio::test]
    async fn wait_and_retry_2fa_times_out_when_code_never_submitted() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let username = "timeout@example.com";
        let session_path = auth::session_file_path(tempdir.path(), username);
        tokio::fs::write(&session_path, br#"{"session_token":"before"}"#)
            .await
            .expect("write initial session metadata");
        let auth_calls = Arc::new(AtomicUsize::new(0));

        let err = wait_and_retry_2fa_with_config(
            tempdir.path(),
            username,
            || {
                let auth_calls = Arc::clone(&auth_calls);
                async move {
                    auth_calls.fetch_add(1, Ordering::SeqCst);
                    Ok::<_, anyhow::Error>(())
                }
            },
            short_2fa_wait_config(Duration::from_millis(30)),
        )
        .await
        .expect_err("never-submitted 2FA must time out");

        let msg = err.to_string();
        assert!(msg.contains("Timed out after"), "msg: {msg}");
        assert!(
            msg.contains(&tempdir.path().display().to_string()),
            "msg: {msg}"
        );
        assert_eq!(auth_calls.load(Ordering::SeqCst), 0);
    }

    /// Build a `PhotoLibrary` stub with a preconfigured mock session.
    fn stub_library(mock: MockPhotosSession) -> PhotoLibrary {
        PhotoLibrary::new_stub(Box::new(mock))
    }

    /// CloudKit folder record for a user album. The albumNameEnc field is
    /// base64-encoded.
    fn folder_record(record_name: &str, album_name: &str) -> serde_json::Value {
        use base64::Engine;
        let encoded = base64::engine::general_purpose::STANDARD.encode(album_name);
        serde_json::json!({
            "recordName": record_name,
            "recordType": "CPLAlbumByPositionLive",
            "fields": {
                "albumNameEnc": {"value": encoded},
                "isDeleted": {"value": false}
            }
        })
    }

    /// Batch album count response.
    fn album_count_response(count: u64) -> serde_json::Value {
        serde_json::json!({
            "batch": [{"records": [{"fields": {"itemCount": {"value": count}}}]}]
        })
    }

    fn names(items: &[&str]) -> BTreeSet<String> {
        items.iter().map(|s| (*s).to_string()).collect()
    }

    fn selection_with_albums(albums: AlbumSelector, unfiled: bool) -> Selection {
        let albums_explicit = !matches!(albums, AlbumSelector::None);
        Selection {
            albums,
            albums_explicit,
            smart_folders: SmartFolderSelector::None,
            smart_folders_explicit: false,
            libraries: crate::selection::LibrarySelector::default(),
            unfiled,
        }
    }

    #[tokio::test]
    async fn resolve_passes_unfiled_only_returns_library_wide_pass() {
        // No albums or smart folders, but unfiled = true: a single library
        // pass with no exclusion (today's `LibraryOnly` default).
        let mock = MockPhotosSession::new().ok(serde_json::json!({"records": []}));
        let library = stub_library(mock);
        let sel = selection_with_albums(AlbumSelector::None, true);

        let plan = resolve_passes(&library, &sel, &[]).await.unwrap();
        assert_eq!(plan.passes.len(), 1);
        assert!(plan.passes[0].exclude_ids.is_empty());
    }

    #[tokio::test]
    async fn resolve_passes_effective_empty_returns_no_passes() {
        // None + None + unfiled=false → no work; sync_loop exits cleanly.
        let mock = MockPhotosSession::new();
        let library = stub_library(mock);
        let sel = selection_with_albums(AlbumSelector::None, false);

        let plan = resolve_passes(&library, &sel, &[]).await.unwrap();
        assert!(plan.passes.is_empty());
    }

    #[tokio::test]
    async fn resolve_passes_named_album_found() {
        let mock = MockPhotosSession::new().ok(serde_json::json!({"records": [
            folder_record("FOLDER_1", "Vacation")
        ]}));
        let library = stub_library(mock);
        let sel = selection_with_albums(
            AlbumSelector::Named {
                included: names(&["Vacation"]),
                excluded: BTreeSet::new(),
            },
            false,
        );

        let plan = resolve_passes(&library, &sel, &[]).await.unwrap();
        assert_eq!(plan.passes.len(), 1);
        assert!(plan.passes[0].exclude_ids.is_empty());
    }

    #[tokio::test]
    async fn resolve_passes_named_album_missing_bails() {
        let mock = MockPhotosSession::new().ok(serde_json::json!({"records": []}));
        let library = stub_library(mock);
        let sel = selection_with_albums(
            AlbumSelector::Named {
                included: names(&["DoesNotExist"]),
                excluded: BTreeSet::new(),
            },
            false,
        );

        let err = resolve_passes(&library, &sel, &[]).await.unwrap_err();
        assert!(err.to_string().contains("not found"), "msg: {err}");
    }

    #[tokio::test]
    async fn resolve_passes_named_smart_folder_in_album_position_bails() {
        // `--album Favorites` should redirect users to `--smart-folder`.
        let mock = MockPhotosSession::new().ok(serde_json::json!({"records": []}));
        let library = stub_library(mock);
        let sel = selection_with_albums(
            AlbumSelector::Named {
                included: names(&["Favorites"]),
                excluded: BTreeSet::new(),
            },
            false,
        );

        let err = resolve_passes(&library, &sel, &[]).await.unwrap_err();
        assert!(err.to_string().contains("smart folder"), "msg: {err}");
    }

    #[tokio::test]
    async fn resolve_passes_all_albums_no_unfiled() {
        let mock = MockPhotosSession::new().ok(serde_json::json!({"records": [
            folder_record("FOLDER_1", "Vacation"),
            folder_record("FOLDER_2", "Summer Trip")
        ]}));
        let library = stub_library(mock);
        let sel = selection_with_albums(
            AlbumSelector::All {
                excluded: BTreeSet::new(),
            },
            false,
        );

        let plan = resolve_passes(&library, &sel, &[]).await.unwrap();
        assert_eq!(plan.passes.len(), 2);
        for p in &plan.passes {
            assert!(p.exclude_ids.is_empty());
        }
    }

    #[tokio::test]
    async fn resolve_passes_all_albums_defers_unfiled_excludes_to_download_phase() {
        let mock = MockPhotosSession::new()
            .ok(serde_json::json!({"records": [folder_record("FOLDER_1", "Vacation")]}));
        let library = stub_library(mock);
        let sel = selection_with_albums(
            AlbumSelector::All {
                excluded: BTreeSet::new(),
            },
            true,
        );

        let plan = resolve_passes(&library, &sel, &[]).await.unwrap();
        assert_eq!(plan.passes.len(), 2, "1 album pass + 1 unfiled pass");
        assert!(plan.passes[0].exclude_ids.is_empty());
        assert!(
            plan.passes[1].exclude_ids.is_empty(),
            "full sync resolves unfiled exclusions concurrently in the download phase"
        );
    }

    #[tokio::test]
    async fn resolve_passes_all_albums_respects_excluded_set() {
        let mock = MockPhotosSession::new().ok(serde_json::json!({"records": [
            folder_record("FOLDER_1", "Vacation"),
            folder_record("FOLDER_2", "Family")
        ]}));
        let library = stub_library(mock);
        let sel = selection_with_albums(
            AlbumSelector::All {
                excluded: names(&["Family"]),
            },
            false,
        );

        let plan = resolve_passes(&library, &sel, &[]).await.unwrap();
        assert_eq!(plan.passes.len(), 1, "Family is filtered out");
    }

    #[tokio::test]
    async fn resolve_passes_smart_folder_named_creates_pass() {
        // No user-created folders; smart folders are seeded by
        // `library.albums()` so we don't need a network response for them.
        let mock = MockPhotosSession::new().ok(serde_json::json!({"records": []}));
        let library = stub_library(mock);
        let sel = Selection {
            albums: AlbumSelector::None,
            albums_explicit: false,
            smart_folders: SmartFolderSelector::Named {
                included: names(&["Favorites"]),
                excluded: BTreeSet::new(),
            },
            smart_folders_explicit: true,
            libraries: crate::selection::LibrarySelector::default(),
            unfiled: false,
        };

        let plan = resolve_passes(&library, &sel, &[]).await.unwrap();
        assert_eq!(plan.passes.len(), 1);
        assert_eq!(plan.passes[0].album.name.as_ref(), "Favorites");
    }

    #[tokio::test]
    async fn resolve_passes_smart_folder_named_unknown_bails() {
        let mock = MockPhotosSession::new();
        let library = stub_library(mock);
        let sel = Selection {
            albums: AlbumSelector::None,
            albums_explicit: false,
            smart_folders: SmartFolderSelector::Named {
                included: names(&["NotASmartFolder"]),
                excluded: BTreeSet::new(),
            },
            smart_folders_explicit: true,
            libraries: crate::selection::LibrarySelector::default(),
            unfiled: false,
        };

        let err = resolve_passes(&library, &sel, &[]).await.unwrap_err();
        assert!(
            err.to_string().contains("not an Apple smart folder"),
            "msg: {err}"
        );
    }

    fn shared_lib_stub(zone: &str) -> PhotoLibrary {
        PhotoLibrary::new_stub_with_zone(Box::new(MockPhotosSession::new()), zone)
    }

    #[tokio::test]
    async fn resolve_passes_shared_library_smart_folder_now_builds_pass() {
        let libs = vec![shared_lib_stub("SharedSync-AAAA1111")];
        let sel = Selection {
            albums: AlbumSelector::None,
            albums_explicit: false,
            smart_folders: SmartFolderSelector::Named {
                included: names(&["Favorites"]),
                excluded: BTreeSet::new(),
            },
            smart_folders_explicit: true,
            libraries: crate::selection::LibrarySelector::default(),
            unfiled: false,
        };
        let ctx = build_collection_context(&sel, &libs).await.unwrap();
        let plan = resolve_passes_for_scope(
            &libs[0],
            &sel,
            PassScope {
                include_albums: false,
                include_smart_folders: true,
                include_unfiled: false,
            },
            &ctx,
            &[],
        )
        .await
        .unwrap();
        assert_eq!(plan.passes.len(), 1);
        assert_eq!(plan.passes[0].album.name.as_ref(), "Favorites");
    }

    #[tokio::test]
    async fn resolve_passes_smart_folder_all_sensitive_truth_table() {
        for include_sensitive in [false, true] {
            let mock = MockPhotosSession::new().ok(serde_json::json!({"records": []}));
            let library = stub_library(mock);
            let sel = Selection {
                albums: AlbumSelector::None,
                albums_explicit: false,
                smart_folders: SmartFolderSelector::All {
                    include_sensitive,
                    excluded: BTreeSet::new(),
                },
                smart_folders_explicit: true,
                libraries: crate::selection::LibrarySelector::default(),
                unfiled: false,
            };

            let plan = resolve_passes(&library, &sel, &[]).await.unwrap();
            let names: BTreeSet<String> = plan
                .passes
                .iter()
                .map(|p| p.album.name.to_string())
                .collect();
            assert!(names.contains("Favorites"), "non-sensitive always present");
            assert_eq!(
                names.contains("Hidden"),
                include_sensitive,
                "Hidden gated on include_sensitive={include_sensitive}"
            );
            assert_eq!(
                names.contains("Recently Deleted"),
                include_sensitive,
                "Recently Deleted gated on include_sensitive={include_sensitive}"
            );
        }
    }

    #[tokio::test]
    async fn resolve_passes_unfiled_with_no_selected_albums_has_empty_exclusion() {
        // None + unfiled=true → library-wide pass, no exclusions.
        let mock = MockPhotosSession::new().ok(serde_json::json!({"records": []}));
        let library = stub_library(mock);
        let sel = selection_with_albums(AlbumSelector::None, true);

        let plan = resolve_passes(&library, &sel, &[]).await.unwrap();
        assert_eq!(plan.passes.len(), 1);
        assert!(plan.passes[0].exclude_ids.is_empty());
    }

    #[tokio::test]
    async fn resolve_passes_all_albums_excluded_typo_bails() {
        // A typo'd exclusion (`!Vacationn`) under the `all` selector is
        // ambiguous: the user intended to exclude something but nothing
        // matches. Bail so they fix the spelling rather than silently
        // sync every album.
        let mock = MockPhotosSession::new().ok(serde_json::json!({"records": [
            folder_record("FOLDER_1", "Vacation"),
            folder_record("FOLDER_2", "Family")
        ]}));
        let library = stub_library(mock);
        let sel = selection_with_albums(
            AlbumSelector::All {
                excluded: names(&["Vacationn"]),
            },
            false,
        );

        let err = resolve_passes(&library, &sel, &[]).await.unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("Vacationn") && msg.contains("Vacation") && msg.contains("Family"),
            "bail must name the typo and the available albums; got {msg}",
        );
    }

    #[tokio::test]
    async fn resolve_passes_named_albums_excluded_typo_bails() {
        // Same rule under Named: even though the typo'd exclusion can't
        // affect the included album, surface it so the user knows their
        // exclusion list is broken.
        let mock = MockPhotosSession::new().ok(serde_json::json!({"records": [
            folder_record("FOLDER_1", "Vacation"),
            folder_record("FOLDER_2", "Family")
        ]}));
        let library = stub_library(mock);
        let sel = selection_with_albums(
            AlbumSelector::Named {
                included: names(&["Vacation"]),
                excluded: names(&["Vacationn"]),
            },
            false,
        );

        let err = resolve_passes(&library, &sel, &[]).await.unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("Vacationn"),
            "bail must name the typo'd exclusion; got {msg}",
        );
    }

    #[tokio::test]
    async fn compute_unfiled_exclude_ids_bails_on_missing_album() {
        // CG-5 defensive: if a selected album is somehow missing from the
        // library album map at exclusion time, surface it instead of
        // silently producing an empty exclusion set (which would let
        // already-filed assets re-download under --unfiled).
        let album_map = std::collections::HashMap::new();
        let selected = vec!["GhostAlbum".to_string()];
        let err = compute_unfiled_exclude_ids(&album_map, &selected)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("GhostAlbum"), "msg: {err}");
    }

    // ── is_misdirected_request tests ──────────────────────────────────

    #[test]
    fn misdirected_request_variant_detected() {
        let err = icloud::error::ICloudError::MisdirectedRequest;
        assert!(is_misdirected_request(&err));
    }

    #[test]
    fn non_421_connection_error_not_misdirected() {
        let err = icloud::error::ICloudError::Connection("HTTP 500 ...".to_string());
        assert!(!is_misdirected_request(&err));
    }

    #[test]
    fn session_expired_not_misdirected() {
        let err = icloud::error::ICloudError::SessionExpired { status: 401 };
        assert!(!is_misdirected_request(&err));
    }

    #[test]
    fn service_not_activated_not_misdirected() {
        let err = icloud::error::ICloudError::ServiceNotActivated {
            code: "ZONE_NOT_FOUND".to_string(),
            reason: "zone not found".to_string(),
        };
        assert!(!is_misdirected_request(&err));
    }

    // ── init_photos_service wiremock tests ────────────────────────────────
    //
    // These tests use a real `Session` (backed by a temp dir) + a wiremock
    // server to exercise the 421 recovery path without hitting iCloud.
    //
    // Path exercised:
    //   init_photos_service
    //     └─ PhotosService::new
    //          └─ PhotoLibrary::new
    //               └─ retry_post → POST /database/1/.../private/records/query
    //
    // On 421 the function resets the HTTP pool and retries once.  A second 421
    // surfaces MisdirectedRequest to the caller.

    /// Build an `AuthResult` whose `ckdatabasews` URL points at `base_url`.
    /// The `Session` is created against a unique temp dir so concurrent tests
    /// don't fight over the same lock file.
    async fn fake_auth_result(base_url: &str) -> auth::AuthResult {
        let dir = tempfile::tempdir().expect("tempdir");
        let session = auth::session::Session::new(dir.path(), "test@example.com", base_url, None)
            .await
            .expect("Session::new");
        // Keep the temp dir alive for the session's lifetime.
        // We intentionally leak it here; the OS cleans it up on process exit
        // and these are short-lived tests.
        #[allow(
            clippy::mem_forget,
            reason = "intentional leak — temp dir must outlive session"
        )]
        std::mem::forget(dir);
        let data = auth::responses::AccountLoginResponse {
            ds_info: None,
            webservices: Some(auth::responses::Webservices {
                ckdatabasews: Some(auth::responses::WebserviceEndpoint {
                    url: base_url.to_owned(),
                }),
            }),
            hsa_challenge_required: false,
            hsa_trusted_browser: true,
            domain_to_use: None,
            has_error: false,
            service_errors: vec![],
            i_cdp_enabled: false,
        };
        auth::AuthResult {
            session,
            data,
            requires_2fa: false,
        }
    }

    /// A `RetryConfig` that never sleeps between attempts — keeps tests fast.
    fn no_delay_retry() -> crate::retry::RetryConfig {
        crate::retry::RetryConfig {
            max_retries: 0,
            base_delay_secs: 0,
            max_delay_secs: 0,
        }
    }

    #[tokio::test]
    async fn init_photos_service_recovers_from_421() {
        use wiremock::matchers::{method, path_regex};
        use wiremock::{Mock, ResponseTemplate};

        let mock_server = crate::start_wiremock_or_skip!();

        // First call → 421; second call → 200 with a valid CheckIndexingState body.
        let success_body = serde_json::json!({
            "records": [{
                "recordName": "CheckIndexingState",
                "recordType": "CheckIndexingState",
                "fields": {
                    "state": {"value": "FINISHED", "type": "STRING"}
                }
            }]
        });

        Mock::given(method("POST"))
            .and(path_regex(
                r"^/database/1/com\.apple\.photos\.cloud/production/private/records/query",
            ))
            .respond_with(ResponseTemplate::new(421))
            .up_to_n_times(1)
            .mount(&mock_server)
            .await;

        Mock::given(method("POST"))
            .and(path_regex(
                r"^/database/1/com\.apple\.photos\.cloud/production/private/records/query",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(&success_body))
            .mount(&mock_server)
            .await;

        let auth_result = fake_auth_result(&mock_server.uri()).await;
        let result = init_photos_service(
            auth_result,
            no_delay_retry(),
            crate::personality::Mode::Off,
            None,
        )
        .await;

        assert!(
            result.is_ok(),
            "expected recovery from single 421, got: {:?}",
            result.err()
        );

        // Verify the mock received exactly 2 requests (the retry happened).
        let received = mock_server.received_requests().await.expect("requests");
        assert_eq!(
            received.len(),
            2,
            "expected exactly 2 requests (initial 421 + retry), got {}",
            received.len()
        );
    }

    #[tokio::test]
    async fn init_photos_service_fails_on_double_421() {
        use wiremock::matchers::{method, path_regex};
        use wiremock::{Mock, ResponseTemplate};

        let mock_server = crate::start_wiremock_or_skip!();

        Mock::given(method("POST"))
            .and(path_regex(
                r"^/database/1/com\.apple\.photos\.cloud/production/private/records/query",
            ))
            .respond_with(ResponseTemplate::new(421))
            .mount(&mock_server)
            .await;

        let auth_result = fake_auth_result(&mock_server.uri()).await;
        let result = init_photos_service(
            auth_result,
            no_delay_retry(),
            crate::personality::Mode::Off,
            None,
        )
        .await;

        let err = result.expect_err("expected double-421 to return Err");
        // The error must downcast to MisdirectedRequest so sync_loop can
        // route it through the SRP re-auth path.
        let icloud_err = err.downcast_ref::<crate::icloud::error::ICloudError>();
        assert!(
            matches!(
                icloud_err,
                Some(crate::icloud::error::ICloudError::MisdirectedRequest)
            ),
            "expected MisdirectedRequest on double-421, got: {err:?}"
        );
    }

    #[tokio::test]
    async fn resolve_passes_named_with_inline_exclude_skips_album() {
        // `--album Vacation --album '!Vacation'` would already bail in the
        // selector parser, but if a programmer constructs that shape
        // directly the resolver must still no-op the included entry.
        let mock = MockPhotosSession::new().ok(serde_json::json!({"records": [
            folder_record("FOLDER_1", "Vacation")
        ]}));
        let library = stub_library(mock);
        let sel = selection_with_albums(
            AlbumSelector::Named {
                included: names(&["Vacation"]),
                excluded: names(&["Vacation"]),
            },
            false,
        );

        let plan = resolve_passes(&library, &sel, &[]).await.unwrap();
        assert!(plan.passes.is_empty());
    }

    #[tokio::test]
    async fn cross_zone_hydration_libraries_skip_fetch_without_album_selection() {
        let sel = Selection {
            albums: AlbumSelector::None,
            albums_explicit: false,
            smart_folders: SmartFolderSelector::Named {
                included: names(&["Favorites"]),
                excluded: BTreeSet::new(),
            },
            smart_folders_explicit: true,
            libraries: crate::selection::LibrarySelector::default(),
            unfiled: false,
        };

        let libraries = resolve_cross_zone_libraries_for_album_hydration(&sel, async {
            panic!("all_libraries must not be polled without album selection")
        })
        .await
        .unwrap();

        assert!(libraries.is_empty());
    }

    #[tokio::test]
    async fn cross_zone_hydration_libraries_propagate_fetch_failure() {
        let sel = selection_with_albums(
            AlbumSelector::Named {
                included: names(&["Vacation"]),
                excluded: BTreeSet::new(),
            },
            false,
        );

        let err = resolve_cross_zone_libraries_for_album_hydration(&sel, async {
            Err::<Vec<PhotoLibrary>, anyhow::Error>(anyhow::anyhow!("library listing failed"))
        })
        .await
        .unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("Could not resolve libraries needed for cross-library album matching"),
            "missing context: {msg}"
        );
        assert!(
            msg.contains("library listing failed"),
            "missing cause: {msg}"
        );
    }

    #[tokio::test]
    async fn resolve_passes_tags_each_pass_with_correct_kind() {
        // The renderer routes per-category template selection on
        // `pass.kind`, so the resolver must tag each pass correctly. Drift
        // here silently misroutes passes to the wrong template.
        let mock = MockPhotosSession::new()
            .ok(serde_json::json!({"records": [folder_record("FOLDER_1", "Vacation")]}))
            .ok(album_count_response(0))
            .ok(serde_json::json!({"records": []}));
        let library = stub_library(mock);
        let sel = Selection {
            albums: AlbumSelector::Named {
                included: names(&["Vacation"]),
                excluded: BTreeSet::new(),
            },
            albums_explicit: true,
            smart_folders: SmartFolderSelector::Named {
                included: names(&["Favorites"]),
                excluded: BTreeSet::new(),
            },
            smart_folders_explicit: true,
            libraries: crate::selection::LibrarySelector::default(),
            unfiled: true,
        };

        let plan = resolve_passes(&library, &sel, &[]).await.unwrap();
        let kinds: Vec<(String, PassKind)> = plan
            .passes
            .iter()
            .map(|p| (p.album.name.to_string(), p.kind))
            .collect();
        assert_eq!(
            kinds,
            vec![
                ("Vacation".to_string(), PassKind::Album),
                ("Favorites".to_string(), PassKind::SmartFolder),
                (library.all().name.to_string(), PassKind::Unfiled),
            ]
        );
    }

    #[tokio::test]
    async fn resolve_passes_threading_from_toml_input() {
        // End-to-end thread of persistent selection config through the
        // production resolver: TOML -> Config::build -> Selection ->
        // resolve_passes. CLI only exposes per-run sync flags in v0.20, so
        // persistent album, smart-folder, unfiled, and library policy should
        // reach the pass planner from TOML.
        use crate::cli::{PasswordArgs, SyncArgs};

        let cookie_dir = tempfile::tempdir().unwrap();
        let toml: crate::config::TomlConfig = toml::from_str(
            r#"
[auth]
username = "u@example.com"

[download]
directory = "/photos"

[filters]
albums = ["all"]
smart_folders = ["Favorites"]
unfiled = false
libraries = ["shared"]
"#,
        )
        .unwrap();
        let globals = crate::config::GlobalArgs {
            username: None,
            domain: None,
            data_dir: Some(cookie_dir.path().to_string_lossy().into_owned()),
        };
        let cfg = crate::config::Config::build(
            &globals,
            &PasswordArgs::default(),
            SyncArgs::default(),
            Some(&toml),
        )
        .unwrap();

        // Sanity-check the TOML → Selection wiring before pinning the pass list.
        assert_eq!(
            cfg.filters.selection.libraries.to_raw(),
            vec!["shared".to_string()]
        );
        assert!(matches!(
            cfg.filters.selection.albums,
            AlbumSelector::All { ref excluded } if excluded.is_empty()
        ));
        assert!(matches!(
            cfg.filters.selection.smart_folders,
            SmartFolderSelector::Named { ref included, ref excluded }
                if included.contains("Favorites") && excluded.is_empty()
        ));
        assert!(!cfg.filters.selection.unfiled);

        // Stub a library exposing one user album ("Vacation") plus the
        // Favorites smart folder. resolve_passes only consumes the first
        // response (`library.albums().await?`); subsequent .ok()s match
        // the resolve_passes_tags_each_pass_with_correct_kind pattern.
        let mock = MockPhotosSession::new()
            .ok(serde_json::json!({"records": [folder_record("FOLDER_VAC", "Vacation")]}))
            .ok(album_count_response(0))
            .ok(serde_json::json!({"records": []}));
        let library = stub_library(mock);

        let plan = resolve_passes(&library, &cfg.filters.selection, &[])
            .await
            .unwrap();
        let pairs: Vec<(String, PassKind)> = plan
            .passes
            .iter()
            .map(|p| (p.album.name.to_string(), p.kind))
            .collect();
        // --album all → 1 Album pass, --smart-folder Favorites → 1 SmartFolder
        // pass, --unfiled false → 0 Unfiled passes.
        assert_eq!(
            pairs,
            vec![
                ("Vacation".to_string(), PassKind::Album),
                ("Favorites".to_string(), PassKind::SmartFolder),
            ],
            "expected [Album(Vacation), SmartFolder(Favorites)] (no unfiled), got {pairs:?}"
        );
    }

    // ── resolve_libraries tests ──────────────────────────────────────

    /// Build a `PhotosService` with a stub primary library plus the named
    /// shared zones. Bypasses CloudKit listing endpoints by pre-populating
    /// the lazy library maps.
    fn photos_service_with_zones(shared_zones: &[&str]) -> icloud::photos::PhotosService {
        use crate::icloud::photos::PhotoLibrary;
        use std::collections::HashMap;

        let primary =
            PhotoLibrary::new_stub_with_zone(Box::new(MockPhotosSession::new()), "PrimarySync");
        let mut shared = HashMap::new();
        for zone in shared_zones {
            shared.insert(
                (*zone).to_string(),
                PhotoLibrary::new_stub_with_zone(Box::new(MockPhotosSession::new()), zone),
            );
        }
        icloud::photos::PhotosService::for_testing_with_libraries(
            Box::new(MockPhotosSession::new()),
            primary,
            HashMap::new(),
            shared,
        )
    }

    fn selector_from(raw: &[&str]) -> crate::selection::LibrarySelector {
        let owned: Vec<String> = raw.iter().map(|s| (*s).to_string()).collect();
        crate::selection::parse_library_selector(&owned).unwrap()
    }

    fn zone_names(libs: &[icloud::photos::PhotoLibrary]) -> Vec<&str> {
        libs.iter()
            .map(icloud::photos::PhotoLibrary::zone_name)
            .collect()
    }

    #[test]
    fn scope_contract_matrix_zone_widening_and_unfiled_scoping() {
        use crate::selection::{AlbumSelector, Selection, SmartFolderSelector};
        use std::collections::BTreeSet;

        let primary_zone = "PrimarySync";
        let shared_zone = "SharedSync-AAAA1111";
        let primary =
            PhotoLibrary::new_stub_with_zone(Box::new(MockPhotosSession::new()), primary_zone);
        let shared =
            PhotoLibrary::new_stub_with_zone(Box::new(MockPhotosSession::new()), shared_zone);
        let all_libraries = vec![primary.clone(), shared.clone()];

        let all_zones = zone_name_set(&all_libraries);
        let library_cases: [(
            &str,
            crate::selection::LibrarySelector,
            Vec<icloud::photos::PhotoLibrary>,
        ); 3] = [
            (
                "primary",
                selector_from(&["primary"]),
                vec![primary.clone()],
            ),
            ("shared", selector_from(&["shared"]), vec![shared.clone()]),
            (
                "all",
                selector_from(&["all"]),
                vec![primary.clone(), shared.clone()],
            ),
        ];
        let album_cases: [(&str, AlbumSelector, bool, bool); 3] = [
            (
                "default",
                AlbumSelector::All {
                    excluded: BTreeSet::new(),
                },
                false,
                true,
            ),
            ("none", AlbumSelector::None, true, false),
            (
                "named",
                AlbumSelector::Named {
                    included: names(&["Vacation"]),
                    excluded: BTreeSet::new(),
                },
                true,
                true,
            ),
        ];
        let smart_cases: [(&str, SmartFolderSelector, bool, bool); 3] = [
            ("default", SmartFolderSelector::None, false, false),
            ("none", SmartFolderSelector::None, true, false),
            (
                "named",
                SmartFolderSelector::Named {
                    included: names(&["Hidden"]),
                    excluded: BTreeSet::new(),
                },
                true,
                true,
            ),
        ];

        let mut matrix_cases = 0usize;
        for (library_label, library_selector, selected_libraries) in &library_cases {
            let selected_zones = zone_name_set(selected_libraries);
            for (album_label, album_selector, albums_explicit, albums_active) in &album_cases {
                for (smart_label, smart_selector, smart_explicit, smart_active) in &smart_cases {
                    for unfiled in [false, true] {
                        matrix_cases += 1;
                        let selection = Selection {
                            albums: album_selector.clone(),
                            albums_explicit: *albums_explicit,
                            smart_folders: smart_selector.clone(),
                            smart_folders_explicit: *smart_explicit,
                            libraries: library_selector.clone(),
                            unfiled,
                        };

                        let collection_libraries =
                            collection_libraries(&selection, selected_libraries, &all_libraries);
                        let collection_zones = zone_name_set(collection_libraries);

                        let expected_collection_zones = if *albums_explicit || *smart_active {
                            all_zones.clone()
                        } else {
                            selected_zones.clone()
                        };
                        assert_eq!(
                            collection_zones, expected_collection_zones,
                            "collection scope mismatch for --library={library_label} --album={album_label} --smart-folder={smart_label} --unfiled={unfiled}"
                        );

                        let primary_scope = pass_scope_for_zone(
                            &selection,
                            primary_zone,
                            &selected_zones,
                            &collection_zones,
                        );
                        let shared_scope = pass_scope_for_zone(
                            &selection,
                            shared_zone,
                            &selected_zones,
                            &collection_zones,
                        );

                        for (zone_name, scope) in
                            [(primary_zone, primary_scope), (shared_zone, shared_scope)]
                        {
                            let expected_unfiled = unfiled && selected_zones.contains(zone_name);
                            let expected_albums = if !albums_active {
                                false
                            } else if *albums_explicit {
                                expected_collection_zones.contains(zone_name)
                            } else {
                                selected_zones.contains(zone_name)
                            };
                            let expected_smart =
                                *smart_active && expected_collection_zones.contains(zone_name);

                            assert_eq!(
                                scope.include_unfiled, expected_unfiled,
                                "unfiled scope mismatch for zone={zone_name}, --library={library_label} --album={album_label} --smart-folder={smart_label} --unfiled={unfiled}"
                            );
                            assert_eq!(
                                scope.include_albums, expected_albums,
                                "album scope mismatch for zone={zone_name}, --library={library_label} --album={album_label} --smart-folder={smart_label} --unfiled={unfiled}"
                            );
                            assert_eq!(
                                scope.include_smart_folders, expected_smart,
                                "smart-folder scope mismatch for zone={zone_name}, --library={library_label} --album={album_label} --smart-folder={smart_label} --unfiled={unfiled}"
                            );
                        }
                    }
                }
            }
        }

        assert_eq!(matrix_cases, 54, "expected full 3x3x3x2 matrix coverage");
    }

    #[tokio::test]
    async fn named_sensitive_smart_folders_widen_across_zones_unfiled_stays_selected() {
        use crate::selection::{AlbumSelector, Selection, SmartFolderSelector};

        for smart_name in ["Hidden", "Recently Deleted"] {
            let primary_zone = "PrimarySync";
            let shared_zone = "SharedSync-AAAA1111";
            let primary =
                PhotoLibrary::new_stub_with_zone(Box::new(MockPhotosSession::new()), primary_zone);
            let shared =
                PhotoLibrary::new_stub_with_zone(Box::new(MockPhotosSession::new()), shared_zone);
            let all_libraries = vec![primary.clone(), shared.clone()];
            let selected_libraries = vec![primary.clone()];

            let selection = Selection {
                albums: AlbumSelector::None,
                albums_explicit: false,
                smart_folders: SmartFolderSelector::Named {
                    included: names(&[smart_name]),
                    excluded: BTreeSet::new(),
                },
                smart_folders_explicit: true,
                libraries: selector_from(&["primary"]),
                unfiled: true,
            };

            let selected_zones = zone_name_set(&selected_libraries);
            let collection = collection_libraries(&selection, &selected_libraries, &all_libraries);
            let collection_zones = zone_name_set(collection);
            let collection_context = build_collection_context(&selection, collection)
                .await
                .unwrap();

            let primary_scope =
                pass_scope_for_zone(&selection, primary_zone, &selected_zones, &collection_zones);
            let shared_scope =
                pass_scope_for_zone(&selection, shared_zone, &selected_zones, &collection_zones);
            assert!(
                primary_scope.include_smart_folders && shared_scope.include_smart_folders,
                "named smart folder should widen to both zones for {smart_name}"
            );
            assert!(primary_scope.include_unfiled);
            assert!(
                !shared_scope.include_unfiled,
                "unfiled should remain selected-library scoped for {smart_name}"
            );

            let primary_plan = resolve_passes_for_scope(
                &primary,
                &selection,
                primary_scope,
                &collection_context,
                &[],
            )
            .await
            .unwrap();
            let shared_plan = resolve_passes_for_scope(
                &shared,
                &selection,
                shared_scope,
                &collection_context,
                &[],
            )
            .await
            .unwrap();

            let primary_smart_names: Vec<String> = primary_plan
                .passes
                .iter()
                .filter(|pass| pass.kind == PassKind::SmartFolder)
                .map(|pass| pass.album.name.to_string())
                .collect();
            let shared_smart_names: Vec<String> = shared_plan
                .passes
                .iter()
                .filter(|pass| pass.kind == PassKind::SmartFolder)
                .map(|pass| pass.album.name.to_string())
                .collect();
            assert_eq!(primary_smart_names, vec![smart_name.to_string()]);
            assert_eq!(shared_smart_names, vec![smart_name.to_string()]);
            assert!(
                primary_plan
                    .passes
                    .iter()
                    .any(|pass| pass.kind == PassKind::Unfiled)
            );
            assert!(
                !shared_plan
                    .passes
                    .iter()
                    .any(|pass| pass.kind == PassKind::Unfiled)
            );
        }
    }

    #[tokio::test]
    async fn resolve_libraries_default_returns_primary_only() {
        let mut ps = photos_service_with_zones(&["SharedSync-AAAA1111", "SharedSync-BBBB2222"]);
        let sel = crate::selection::LibrarySelector::default();
        let libs = resolve_libraries(&sel, &mut ps).await.unwrap();
        assert_eq!(zone_names(&libs), vec!["PrimarySync"]);
    }

    #[tokio::test]
    async fn resolve_libraries_all_returns_every_zone() {
        let mut ps = photos_service_with_zones(&["SharedSync-AAAA1111", "SharedSync-BBBB2222"]);
        let sel = selector_from(&["all"]);
        let libs = resolve_libraries(&sel, &mut ps).await.unwrap();
        let mut names = zone_names(&libs);
        names.sort_unstable();
        assert_eq!(
            names,
            vec!["PrimarySync", "SharedSync-AAAA1111", "SharedSync-BBBB2222"]
        );
    }

    #[tokio::test]
    async fn resolve_libraries_shared_only_excludes_primary() {
        let mut ps = photos_service_with_zones(&["SharedSync-AAAA1111", "SharedSync-BBBB2222"]);
        let sel = selector_from(&["shared"]);
        let libs = resolve_libraries(&sel, &mut ps).await.unwrap();
        let mut names = zone_names(&libs);
        names.sort_unstable();
        assert_eq!(names, vec!["SharedSync-AAAA1111", "SharedSync-BBBB2222"]);
    }

    #[tokio::test]
    async fn resolve_libraries_named_zone_returns_only_that_zone() {
        let mut ps = photos_service_with_zones(&["SharedSync-AAAA1111", "SharedSync-BBBB2222"]);
        let sel = selector_from(&["SharedSync-AAAA1111"]);
        let libs = resolve_libraries(&sel, &mut ps).await.unwrap();
        assert_eq!(zone_names(&libs), vec!["SharedSync-AAAA1111"]);
    }

    #[tokio::test]
    async fn resolve_libraries_named_zone_matches_truncated_form() {
        let mut ps =
            photos_service_with_zones(&["SharedSync-AAAA1111-2222-3333-4444-555555555555"]);
        let sel = selector_from(&["SharedSync-AAAA1111"]);
        let libs = resolve_libraries(&sel, &mut ps).await.unwrap();
        assert_eq!(
            zone_names(&libs),
            vec!["SharedSync-AAAA1111-2222-3333-4444-555555555555"]
        );
    }

    #[tokio::test]
    async fn resolve_libraries_primary_plus_named_returns_both() {
        let mut ps = photos_service_with_zones(&["SharedSync-AAAA1111", "SharedSync-BBBB2222"]);
        let sel = selector_from(&["primary", "SharedSync-AAAA1111"]);
        let libs = resolve_libraries(&sel, &mut ps).await.unwrap();
        let mut names = zone_names(&libs);
        names.sort_unstable();
        assert_eq!(names, vec!["PrimarySync", "SharedSync-AAAA1111"]);
    }

    #[tokio::test]
    async fn resolve_libraries_multiple_named_zones() {
        let mut ps = photos_service_with_zones(&[
            "SharedSync-AAAA1111",
            "SharedSync-BBBB2222",
            "SharedSync-CCCC3333",
        ]);
        let sel = selector_from(&["SharedSync-AAAA1111", "SharedSync-BBBB2222"]);
        let libs = resolve_libraries(&sel, &mut ps).await.unwrap();
        let mut names = zone_names(&libs);
        names.sort_unstable();
        assert_eq!(names, vec!["SharedSync-AAAA1111", "SharedSync-BBBB2222"]);
    }

    #[tokio::test]
    async fn resolve_libraries_truncated_collision_bails() {
        // CloudKit UUIDs are not guaranteed unique on the leading 8 hex
        // characters that `truncate_library_zone` keeps. Two distinct
        // shared zones whose UUIDs collide on that prefix render to the
        // same `{library}` segment on disk; without a resolver-time bail,
        // multi-library sync silently overwrites one zone's bytes with
        // the other's. State DB stays per-zone-distinct under the v8 PK
        // so the corruption is invisible until users notice missing
        // files. The repo's own paths.rs unit test pins the prefix
        // collision invariant; this test pins the bail that relies on it.
        let mut ps = photos_service_with_zones(&[
            "SharedSync-AAAA1111-EEEE-2222-3333-444444444444",
            "SharedSync-AAAA1111-FFFF-5555-6666-777777777777",
        ]);
        let sel = selector_from(&["all"]);
        let err = resolve_libraries(&sel, &mut ps)
            .await
            .expect_err("two zones sharing the 8-hex truncation prefix must bail");
        let msg = err.to_string();
        assert!(
            msg.contains("SharedSync-AAAA1111-EEEE-2222-3333-444444444444")
                && msg.contains("SharedSync-AAAA1111-FFFF-5555-6666-777777777777"),
            "bail must name both colliding full UUIDs so the user can pin a longer form; got: {msg}"
        );
        assert!(
            msg.contains("SharedSync-AAAA1111"),
            "bail must include the truncated form that's the collision point; got: {msg}"
        );
    }

    #[tokio::test]
    async fn resolve_libraries_truncated_collision_with_explicit_zone_does_not_bail() {
        // The bail is only relevant when *both* colliding zones land in
        // the chosen set. If the user pins one of them via a longer
        // `--library SharedSync-AAAA1111-EEEE`, the other is excluded
        // and the on-disk paths can't collide, so the resolver must not
        // bail.
        let mut ps = photos_service_with_zones(&[
            "SharedSync-AAAA1111-EEEE-2222-3333-444444444444",
            "SharedSync-AAAA1111-FFFF-5555-6666-777777777777",
        ]);
        let sel = selector_from(&["SharedSync-AAAA1111-EEEE-2222-3333-444444444444"]);
        let libs = resolve_libraries(&sel, &mut ps)
            .await
            .expect("explicit single-zone selection must not trip the collision bail");
        assert_eq!(
            zone_names(&libs),
            vec!["SharedSync-AAAA1111-EEEE-2222-3333-444444444444"]
        );
    }

    #[tokio::test]
    async fn resolve_libraries_exclusion_drops_named_zone() {
        let mut ps = photos_service_with_zones(&["SharedSync-AAAA1111", "SharedSync-BBBB2222"]);
        let sel = selector_from(&["all", "!SharedSync-AAAA1111"]);
        let libs = resolve_libraries(&sel, &mut ps).await.unwrap();
        let mut names = zone_names(&libs);
        names.sort_unstable();
        assert_eq!(names, vec!["PrimarySync", "SharedSync-BBBB2222"]);
    }

    #[tokio::test]
    async fn resolve_libraries_exclusion_via_shared_sentinel() {
        let mut ps = photos_service_with_zones(&["SharedSync-AAAA1111", "SharedSync-BBBB2222"]);
        let sel = selector_from(&["all", "!shared"]);
        let libs = resolve_libraries(&sel, &mut ps).await.unwrap();
        assert_eq!(zone_names(&libs), vec!["PrimarySync"]);
    }

    #[tokio::test]
    async fn resolve_libraries_named_miss_bails_with_helpful_error() {
        let mut ps = photos_service_with_zones(&["SharedSync-AAAA1111"]);
        let sel = selector_from(&["SharedSync-NOPE"]);
        let err = resolve_libraries(&sel, &mut ps).await.unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("SharedSync-NOPE"),
            "miss must name the input: {msg}"
        );
        assert!(
            msg.contains("Available zones") || msg.contains("kei list libraries"),
            "miss must point at discovery: {msg}"
        );
    }

    #[tokio::test]
    async fn resolve_libraries_zero_match_after_exclusion_bails() {
        let mut ps = photos_service_with_zones(&[]);
        let sel = selector_from(&["primary", "!primary"]);
        let err = resolve_libraries(&sel, &mut ps).await.unwrap_err();
        assert!(
            err.to_string().contains("did not select any libraries"),
            "unexpected error: {err}"
        );
    }
}
