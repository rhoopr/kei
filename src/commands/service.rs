use std::path::Path;
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
) -> anyhow::Result<(auth::SharedSession, icloud::photos::PhotosService)> {
    auth_result
        .session
        .bind_principal(&auth_result.data, None)?;
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
    let session_box: Box<dyn icloud::photos::PhotosSession> = Box::new(shared_session.clone());

    tracing::debug!("Initializing photos service...");
    match icloud::photos::PhotosService::new(
        ckdatabasews_url.clone(),
        session_box,
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

    let session_box: Box<dyn icloud::photos::PhotosSession> = Box::new(shared_session.clone());
    let service = match icloud::photos::PhotosService::new(
        ckdatabasews_url.clone(),
        session_box,
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
        auth::AuthPersistence::Writable,
    )
    .await?;

    let mut session = shared_session.write().await;
    replace_authenticated_session(&mut session, new_auth)?;
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

fn replace_authenticated_session(
    current: &mut auth::session::Session,
    mut replacement: auth::AuthResult,
) -> anyhow::Result<()> {
    replacement
        .session
        .bind_principal(&replacement.data, current.principal())?;
    *current = replacement.session;
    Ok(())
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

fn auth_signal_paths(cookie_dir: &Path, username: &str, realm: &str) -> [std::path::PathBuf; 3] {
    let [cookie, session, cache] = auth::session::persisted_auth_files(cookie_dir, username, realm);
    [session, cookie, cache]
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
             Complete the 2FA steps printed above and restart the service or rerun kei. Error: {e}",
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
    realm: &str,
) -> anyhow::Result<()> {
    let signal_paths = auth_signal_paths(cookie_dir, username, realm);
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
                 state under {path}. Complete the 2FA steps printed above and restart the service or rerun kei.",
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
    realm: &str,
) -> anyhow::Result<T>
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<T>>,
{
    wait_and_retry_2fa_with_config(
        cookie_dir,
        username,
        auth_fn,
        TwoFaWaitConfig::default(),
        realm,
    )
    .await
}

async fn wait_and_retry_2fa_with_config<T, F, Fut>(
    cookie_dir: &Path,
    username: &str,
    auth_fn: F,
    config: TwoFaWaitConfig,
    realm: &str,
) -> anyhow::Result<T>
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<T>>,
{
    let deadline = tokio::time::Instant::now() + config.timeout;

    loop {
        wait_for_2fa_submit_until(cookie_dir, username, config, deadline, realm).await?;

        // Invalidate the validation cache so authenticate() actually checks
        // with Apple instead of returning stale cached data from before 2FA.
        let cache_path = auth::validation_cache_file_path(cookie_dir, username, realm);
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

    // Default selection qualifies primary ownership through cached private
    // discovery. It does not need the shared database listing.
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

/// Match a `--library` entry (full zone name or truncated 8-char form)
/// against a live zone, case-insensitive. The truncated form is what
/// `{library}` renders into paths, so users can copy a path segment and
/// paste it back into `--library`.
fn library_entry_matches_zone(entry: &str, zone: &str, truncated: &str) -> bool {
    entry.eq_ignore_ascii_case(zone) || entry.eq_ignore_ascii_case(truncated)
}
