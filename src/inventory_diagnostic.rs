//! Early command path. No Config::build, catalogue, media, hooks or credential provider.
use crate::auth::{responses::ValidationCache, session::Session};
use crate::icloud::photos::{PhotosService, diagnostic::DiagnosticSession};
use serde_json::{Value, json};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio_util::sync::CancellationToken;

const INPUT_BYTES: u64 = 1024 * 1024;
const OUTPUT_BYTES: usize = 64 * 1024;
const DEADLINE: Duration = Duration::from_secs(30 * 60);

struct Scratch(PathBuf);
impl Drop for Scratch {
    fn drop(&mut self) {
        // Bounded private scratch only. Never holds an input, catalogue or media path.
        let path = self.0.clone();
        let _cleanup = tokio::task::spawn_blocking(move || {
            let _result = std::fs::remove_dir_all(path);
        });
    }
}

async fn bounded_file(path: &Path) -> anyhow::Result<Vec<u8>> {
    let before = tokio::fs::symlink_metadata(path).await?;
    anyhow::ensure!(
        before.is_file() && before.len() <= INPUT_BYTES,
        "diagnostic_input_unavailable"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        anyhow::ensure!(before.nlink() == 1, "diagnostic_input_unavailable");
    }
    let mut options = tokio::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    let file = options.open(path).await?;
    let opened = file.metadata().await?;
    anyhow::ensure!(
        opened.is_file()
            && opened.len() == before.len()
            && opened.modified()? == before.modified()?,
        "diagnostic_input_changed"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        anyhow::ensure!(
            opened.ino() == before.ino() && opened.dev() == before.dev() && opened.nlink() == 1,
            "diagnostic_input_changed"
        );
    }
    let mut bytes = Vec::new();
    file.take(INPUT_BYTES + 1).read_to_end(&mut bytes).await?;
    let after = tokio::fs::symlink_metadata(path).await?;
    anyhow::ensure!(
        after.is_file()
            && bytes.len() as u64 <= INPUT_BYTES
            && before.len() == after.len()
            && before.modified()? == after.modified()?,
        "diagnostic_input_changed"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        anyhow::ensure!(
            before.ino() == after.ino() && before.dev() == after.dev(),
            "diagnostic_input_changed"
        );
    }
    Ok(bytes)
}

fn input_stamp(
    meta: &std::fs::Metadata,
) -> std::io::Result<(u64, std::time::SystemTime, u64, u64)> {
    #[cfg(unix)]
    let identity = {
        use std::os::unix::fs::MetadataExt;
        (meta.dev(), meta.ino())
    };
    #[cfg(not(unix))]
    let identity = (0, 0);
    Ok((meta.len(), meta.modified()?, identity.0, identity.1))
}

async fn scratch() -> anyhow::Result<Scratch> {
    let path = std::env::temp_dir().join(format!("kei-inventory-{}", uuid::Uuid::new_v4()));
    #[cfg(unix)]
    {
        let mut builder = tokio::fs::DirBuilder::new();
        builder.mode(0o700).create(&path).await?;
    }
    #[cfg(not(unix))]
    tokio::fs::create_dir(&path).await?;
    Ok(Scratch(path))
}

fn endpoint_allowed(root: &str, loopback: bool) -> bool {
    let Ok(url) = url::Url::parse(root) else {
        return false;
    };
    if !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || !matches!(url.path(), "" | "/")
    {
        return false;
    }
    #[cfg(test)]
    if loopback && url.scheme() == "http" && url.host_str() == Some("127.0.0.1") {
        return true;
    }
    let _unused_in_release = loopback;
    let Some(host) = url.host_str() else {
        return false;
    };
    let host = host
        .strip_suffix(".icloud.com")
        .or_else(|| host.strip_suffix(".icloud.com.cn"));
    let qualified = host.is_some_and(|prefix| {
        prefix == "ckdatabasews"
            || prefix
                .strip_prefix('p')
                .and_then(|prefix| prefix.strip_suffix("-ckdatabasews"))
                .is_some_and(|number| {
                    !number.is_empty() && number.bytes().all(|byte| byte.is_ascii_digit())
                })
    });
    url.scheme() == "https" && url.port().is_none() && qualified
}

async fn collect(
    config_path: &Path,
    cancel: &CancellationToken,
    loopback: bool,
) -> anyhow::Result<Value> {
    let config = if tokio::fs::try_exists(config_path).await? {
        let bytes = bounded_file(config_path).await?;
        Some(toml::from_str::<crate::config::TomlConfig>(
            std::str::from_utf8(&bytes)?,
        )?)
    } else {
        None
    };
    let globals = crate::config::GlobalArgs::from_bootstrap_env();
    let (username, _, domain, source) = crate::config::resolve_auth(
        &globals,
        &crate::cli::PasswordArgs::default(),
        config.as_ref(),
    )?;
    anyhow::ensure!(
        !username.trim().is_empty(),
        "diagnostic_cached_session_unavailable"
    );
    let files = crate::auth::session::persisted_auth_files(&source, &username, domain.as_str());
    let mut stamps = Vec::new();
    for path in &files {
        stamps.push(input_stamp(&tokio::fs::symlink_metadata(path).await?)?);
    }
    let scratch = scratch().await?;
    for path in &files {
        let bytes = bounded_file(path).await?;
        let name = path
            .file_name()
            .ok_or_else(|| anyhow::anyhow!("diagnostic_input_unavailable"))?;
        let mut options = tokio::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o600);
        let mut file = options.open(scratch.0.join(name)).await?;
        use tokio::io::AsyncWriteExt;
        file.write_all(&bytes).await?;
        file.flush().await?;
    }
    for (path, stamp) in files.iter().zip(stamps) {
        anyhow::ensure!(
            input_stamp(&tokio::fs::symlink_metadata(path).await?)? == stamp,
            "diagnostic_input_changed"
        );
    }
    let cache: ValidationCache = serde_json::from_slice(
        &bounded_file(
            &scratch.0.join(
                files[2]
                    .file_name()
                    .ok_or_else(|| anyhow::anyhow!("diagnostic_input_unavailable"))?,
            ),
        )
        .await?,
    )?;
    let age = chrono::Utc::now()
        .timestamp()
        .checked_sub(cache.validated_at);
    anyhow::ensure!(
        age.is_some_and(
            |age| (0..=crate::auth::responses::VALIDATION_CACHE_GRACE_SECS).contains(&age)
        ),
        "diagnostic_cached_session_unavailable"
    );
    let endpoints = crate::auth::endpoints::Endpoints::for_domain(domain.as_str())?;
    let mut session = Session::new(&scratch.0, &username, endpoints.home, Some(30)).await?;
    anyhow::ensure!(
        session
            .session_data
            .get("session_token")
            .is_some_and(|token| !token.trim().is_empty()),
        "diagnostic_cached_session_unavailable"
    );
    session.bind_principal(&cache.account_data, None)?;
    anyhow::ensure!(
        session.principal().is_some() && !cache.account_data.i_cdp_enabled,
        "diagnostic_cached_session_unavailable"
    );
    let root = cache
        .account_data
        .webservices
        .as_ref()
        .and_then(|ws| ws.ckdatabasews.as_ref())
        .map(|endpoint| endpoint.url.as_str())
        .ok_or_else(|| anyhow::anyhow!("diagnostic_endpoint_unavailable"))?;
    anyhow::ensure!(
        endpoint_allowed(root, loopback),
        "diagnostic_endpoint_unavailable"
    );
    let root = root.trim_end_matches('/');
    let params = crate::commands::build_photos_params(
        session.client_id().unwrap_or_default(),
        cache
            .account_data
            .ds_info
            .as_ref()
            .and_then(|info| info.dsid.as_deref()),
    );
    let transport = DiagnosticSession::new(session.diagnostic_client()?, root);
    let retry = crate::retry::RetryConfig {
        max_retries: 0,
        ..Default::default()
    };
    let inventory = async {
        let mut service =
            PhotosService::new(root.to_owned(), Box::new(transport.clone()), params, retry).await?;
        let library = service.get_library("PrimarySync").await?;
        anyhow::ensure!(
            library.is_qualified_private_primary() && library.last_indexing_finished(),
            "diagnostic_primary_unavailable"
        );
        Ok::<_, anyhow::Error>(library.all().missing_reference_inventory(cancel).await)
    };
    let mut report = tokio::select! {
        biased;
        () = cancel.cancelled() => json!({"scan":"incomplete","reason":"cancelled"}),
        result = inventory => result.unwrap_or_else(|_private_error| json!({"scan":"incomplete","reason":"discovery_unavailable"})),
    };
    if let Some(report) = report.as_object_mut() {
        report.insert("transport".into(), transport.counts());
        report.insert("limits".into(), json!({"pages":1000,"records":200000,"page_bytes":67108864,
            "response_bytes":1073741824,"retained_bytes":268435456,"shown_groups":32,"output_bytes":OUTPUT_BYTES,"async_deadline_seconds":1800}));
    }
    drop(session);
    drop(scratch);
    Ok(report)
}

pub(crate) async fn run(config_path: &Path) -> anyhow::Result<()> {
    run_with_policy(config_path, false, DEADLINE).await
}

async fn run_with_policy(
    config_path: &Path,
    loopback: bool,
    deadline: Duration,
) -> anyhow::Result<()> {
    let cancel = CancellationToken::new();
    let report = tokio::select! {
        biased;
        _signal = tokio::signal::ctrl_c() => json!({"scan":"incomplete","reason":"cancelled"}),
        result = tokio::time::timeout(deadline, collect(config_path, &cancel, loopback)) => match result {
            Err(_) => json!({"scan":"incomplete","reason":"deadline"}),
            Ok(Err(_private_error)) => json!({"scan":"incomplete","reason":"cached_input_unavailable"}),
            Ok(Ok(report)) => report,
        }
    };
    let mut bytes = serde_json::to_vec(&report)?;
    anyhow::ensure!(bytes.len() < OUTPUT_BYTES, "diagnostic_output_limit");
    bytes.push(b'\n');
    std::io::stdout().lock().write_all(&bytes)?;
    anyhow::ensure!(
        report.get("scan").and_then(Value::as_str) == Some("eof_observed"),
        "missing-reference inventory incomplete"
    );
    Ok(())
}

#[cfg(test)]
mod tests;
