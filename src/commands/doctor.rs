#![allow(
    clippy::print_stdout,
    reason = "CLI diagnostics command whose primary purpose is to print a report to stdout"
)]

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::auth;
use crate::cli;
use crate::config;
use crate::state;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum CheckStatus {
    Ok,
    Warning,
    Error,
    Skipped,
}

#[derive(Debug, Clone, Serialize)]
struct DoctorCheck {
    name: &'static str,
    status: CheckStatus,
    message: String,
}

#[derive(Debug, Clone, Serialize)]
struct DoctorReport {
    version: &'static str,
    install_method: &'static str,
    config_path: String,
    data_dir: String,
    report_path: Option<String>,
    checks: Vec<DoctorCheck>,
}

#[derive(Debug, Default)]
struct Redactor {
    secrets: Vec<String>,
}

impl Redactor {
    fn from_config(globals: &config::GlobalArgs, toml: Option<&config::TomlConfig>) -> Self {
        let mut redactor = Self::default();
        if let Some(username) = &globals.username {
            redactor.add(username);
        }
        if let Some(auth) = toml.and_then(|t| t.auth.as_ref()) {
            if let Some(username) = &auth.username {
                redactor.add(username);
            }
            if let Some(password) = &auth.password {
                redactor.add(password);
            }
            if let Some(password_file) = &auth.password_file {
                redactor.add(password_file);
            }
            if let Some(password_command) = &auth.password_command {
                redactor.add(password_command);
            }
        }
        redactor
    }

    fn add(&mut self, value: &str) {
        if value.len() >= 3 && !self.secrets.iter().any(|s| s == value) {
            self.secrets.push(value.to_string());
        }
    }

    fn redact(&self, input: &str) -> String {
        let mut out = input.to_string();
        for secret in &self.secrets {
            out = out.replace(secret, "<redacted>");
        }
        out = redact_sensitive_assignment_lines(&out);
        out = redact_inline_secret_values(&out);
        redact_email_like_values(&out)
    }

    fn redact_report(&self, mut report: DoctorReport) -> DoctorReport {
        report.config_path = self.redact(&report.config_path);
        report.data_dir = self.redact(&report.data_dir);
        report.report_path = report.report_path.map(|p| self.redact(&p));
        for check in &mut report.checks {
            check.message = self.redact(&check.message);
        }
        report
    }
}

fn redact_inline_secret_values(input: &str) -> String {
    let mut out = input.to_string();
    for key in [
        "password",
        "session_token",
        "session",
        "cookie",
        "bearer",
        "token",
        "trust_token",
    ] {
        out = redact_after_marker(&out, key, '=');
        out = redact_after_marker(&out, key, ':');
    }
    redact_bearer_values(&out)
}

fn redact_sensitive_assignment_lines(input: &str) -> String {
    const SENSITIVE_KEYS: [&str; 12] = [
        "apple_id", "bearer", "cookie", "password", "private", "record", "session", "token",
        "trust", "url", "username", "webhook",
    ];
    input
        .lines()
        .map(|line| {
            let lower = line.to_ascii_lowercase();
            if !SENSITIVE_KEYS.iter().any(|key| lower.contains(key)) {
                return line.to_string();
            }
            let Some(separator) = line.find('=').or_else(|| line.find(':')) else {
                return line.to_string();
            };
            let (prefix, _) = line.split_at(separator + 1);
            format!("{prefix} <redacted>")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn redact_after_marker(input: &str, key: &str, separator: char) -> String {
    let marker = format!("{key}{separator}");
    let lower = input.to_ascii_lowercase();
    let Some(start) = lower.find(&marker) else {
        return input.to_string();
    };
    let value_start = start + marker.len();
    let Some(tail) = input.get(value_start..) else {
        return input.to_string();
    };
    let value_end = tail
        .find(|c: char| c.is_whitespace() || c == ',' || c == '}')
        .map_or(input.len(), |offset| value_start + offset);
    let Some(prefix) = input.get(..value_start) else {
        return input.to_string();
    };
    let Some(suffix) = input.get(value_end..) else {
        return input.to_string();
    };
    let mut out = String::with_capacity(input.len());
    out.push_str(prefix);
    out.push_str("<redacted>");
    out.push_str(suffix);
    out
}

fn redact_email_like_values(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut word = String::new();
    for ch in input.chars() {
        if ch.is_whitespace() {
            push_redacted_email_word(&mut out, &word);
            word.clear();
            out.push(ch);
        } else {
            word.push(ch);
        }
    }
    push_redacted_email_word(&mut out, &word);
    out
}

fn push_redacted_email_word(out: &mut String, word: &str) {
    let trimmed = word.trim_matches(|c: char| c.is_ascii_punctuation() && c != '@' && c != '.');
    if trimmed.contains('@') && trimmed.contains('.') {
        out.push_str("<redacted>");
    } else {
        out.push_str(word);
    }
}

fn redact_bearer_values(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut word = String::new();
    let mut redact_next = false;
    for ch in input.chars() {
        if ch.is_whitespace() {
            push_redacted_bearer_word(&mut out, &word, &mut redact_next);
            word.clear();
            out.push(ch);
        } else {
            word.push(ch);
        }
    }
    push_redacted_bearer_word(&mut out, &word, &mut redact_next);
    out
}

fn push_redacted_bearer_word(out: &mut String, word: &str, redact_next: &mut bool) {
    if word.is_empty() {
        return;
    }
    if *redact_next {
        out.push_str("<redacted>");
        *redact_next = false;
        return;
    }
    out.push_str(word);
    if word.eq_ignore_ascii_case("bearer") {
        *redact_next = true;
    }
}

pub(crate) async fn run_doctor(
    args: cli::DoctorArgs,
    globals: &config::GlobalArgs,
    toml: Option<&config::TomlConfig>,
    config_path: &Path,
    config_load_error: Option<String>,
) -> anyhow::Result<()> {
    let (username, _, domain, cookie_dir) =
        config::resolve_auth(globals, &cli::PasswordArgs::default(), toml);
    let report_path = toml
        .and_then(|t| t.report.as_ref())
        .and_then(|r| r.json.as_deref())
        .map(config::expand_tilde);
    let mut redactor = Redactor::from_config(globals, toml);
    redactor.add(&username);

    let mut checks = Vec::new();
    checks.push(check_config_parse(config_load_error.as_deref()));
    checks.push(check_download_dir(toml));
    checks.push(check_state_db(&username, &cookie_dir, domain.as_str()).await);
    checks.push(check_session_presence(
        &username,
        &cookie_dir,
        domain.as_str(),
    ));
    checks.push(check_health(&cookie_dir));
    checks.push(check_report(report_path.as_deref()));
    if args.live {
        checks.push(check_live_session(&username, domain.as_str(), &cookie_dir).await);
    }

    let report = DoctorReport {
        version: env!("CARGO_PKG_VERSION"),
        install_method: detect_install_method(),
        config_path: config_path.display().to_string(),
        data_dir: cookie_dir.display().to_string(),
        report_path: report_path.as_ref().map(|p| p.display().to_string()),
        checks,
    };
    let report = redactor.redact_report(report);
    let has_error = report
        .checks
        .iter()
        .any(|check| check.status == CheckStatus::Error);

    if args.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print_human(&report);
    }

    if has_error {
        anyhow::bail!("doctor found local errors");
    }
    Ok(())
}

fn check_config_parse(error: Option<&str>) -> DoctorCheck {
    match error {
        Some(error) => DoctorCheck {
            name: "config_parse",
            status: CheckStatus::Error,
            message: error.to_string(),
        },
        None => DoctorCheck {
            name: "config_parse",
            status: CheckStatus::Ok,
            message: "config parsed".to_string(),
        },
    }
}

fn check_download_dir(toml: Option<&config::TomlConfig>) -> DoctorCheck {
    let Some(raw_dir) = toml
        .and_then(|t| t.download.as_ref())
        .and_then(|d| d.directory.as_deref())
    else {
        return DoctorCheck {
            name: "download_dir",
            status: CheckStatus::Skipped,
            message: "no [download].directory configured".to_string(),
        };
    };
    let dir = config::expand_tilde(raw_dir);
    let probe = dir.join(format!(".kei-doctor-{}.tmp", std::process::id()));
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe)
    {
        Ok(_) => {
            let _ = std::fs::remove_file(&probe);
            DoctorCheck {
                name: "download_dir",
                status: CheckStatus::Ok,
                message: format!("download directory is writable: {}", dir.display()),
            }
        }
        Err(e) => DoctorCheck {
            name: "download_dir",
            status: CheckStatus::Error,
            message: format!("download directory probe failed at {}: {e}", dir.display()),
        },
    }
}

async fn check_state_db(username: &str, cookie_dir: &Path, realm: &str) -> DoctorCheck {
    let Some(path) = state_db_path(username, cookie_dir, realm) else {
        return DoctorCheck {
            name: "state_db",
            status: CheckStatus::Skipped,
            message: "no username configured, so no state DB path could be derived".to_string(),
        };
    };
    if let Err(error) = state::db::account::state_path(cookie_dir, username, realm).await {
        return DoctorCheck {
            name: "state_db",
            status: CheckStatus::Error,
            message: error.to_string(),
        };
    }
    if !path.exists() {
        return DoctorCheck {
            name: "state_db",
            status: CheckStatus::Warning,
            message: format!("state DB does not exist yet: {}", path.display()),
        };
    }
    let summary = match state::SqliteStateDb::open_owned(
        &path,
        &state::db::account::AccountOwner::configured(username, realm),
    )
    .await
    {
        Ok(db) => db.get_summary().await,
        Err(e) => Err(e),
    };
    match summary {
        Ok(summary) => DoctorCheck {
            name: "state_db",
            status: CheckStatus::Ok,
            message: format!(
                "state DB opened: {} assets, {} pending, {} failed",
                summary.total_assets, summary.pending, summary.failed
            ),
        },
        Err(e) => DoctorCheck {
            name: "state_db",
            status: CheckStatus::Error,
            message: format!("state DB read failed at {}: {e}", path.display()),
        },
    }
}

fn check_session_presence(username: &str, cookie_dir: &Path, realm: &str) -> DoctorCheck {
    if username.is_empty() {
        return DoctorCheck {
            name: "session",
            status: CheckStatus::Skipped,
            message: "no username configured, so no session path could be derived".to_string(),
        };
    }
    let session_path = auth::session_file_path(cookie_dir, username, realm);
    let cookie_path = auth::cookiejar_file_path(cookie_dir, username, realm);
    let present = session_path.exists() || cookie_path.exists();
    DoctorCheck {
        name: "session",
        status: if present {
            CheckStatus::Ok
        } else {
            CheckStatus::Warning
        },
        message: if present {
            "local session or cookie jar is present".to_string()
        } else {
            "no local session or cookie jar found".to_string()
        },
    }
}

async fn check_live_session(username: &str, domain: &str, cookie_dir: &Path) -> DoctorCheck {
    if username.is_empty() {
        return DoctorCheck {
            name: "live_session",
            status: CheckStatus::Skipped,
            message: "no username configured, so live session validation was skipped".to_string(),
        };
    }

    let endpoints = match auth::endpoints::Endpoints::for_domain(domain) {
        Ok(endpoints) => endpoints,
        Err(e) => {
            return DoctorCheck {
                name: "live_session",
                status: CheckStatus::Error,
                message: format!("could not resolve iCloud auth endpoints: {e}"),
            };
        }
    };
    let mut session =
        match auth::session::Session::new(cookie_dir, username, endpoints.home, None).await {
            Ok(session) => session,
            Err(e) => {
                return DoctorCheck {
                    name: "live_session",
                    status: CheckStatus::Error,
                    message: format!("could not open local session for live validation: {e}"),
                };
            }
        };

    match auth::validate_session(&mut session, domain).await {
        Ok(true) => DoctorCheck {
            name: "live_session",
            status: CheckStatus::Ok,
            message: "saved iCloud session validated".to_string(),
        },
        Ok(false) => DoctorCheck {
            name: "live_session",
            status: CheckStatus::Warning,
            message: "saved iCloud session is not currently valid; run `kei login`".to_string(),
        },
        Err(e) => DoctorCheck {
            name: "live_session",
            status: CheckStatus::Warning,
            message: format!("live iCloud session validation failed: {e}"),
        },
    }
}

fn check_health(cookie_dir: &Path) -> DoctorCheck {
    let path = cookie_dir.join("health.json");
    check_json_file("health", &path)
}

fn check_report(path: Option<&Path>) -> DoctorCheck {
    match path {
        Some(path) => check_json_file("report", path),
        None => DoctorCheck {
            name: "report",
            status: CheckStatus::Skipped,
            message: "no [report].json configured".to_string(),
        },
    }
}

fn check_json_file(name: &'static str, path: &Path) -> DoctorCheck {
    match std::fs::read_to_string(path) {
        Ok(contents) => match serde_json::from_str::<serde_json::Value>(&contents) {
            Ok(_) => DoctorCheck {
                name,
                status: CheckStatus::Ok,
                message: format!("valid JSON at {}", path.display()),
            },
            Err(e) => DoctorCheck {
                name,
                status: CheckStatus::Warning,
                message: format!("could not parse JSON at {}: {e}", path.display()),
            },
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => DoctorCheck {
            name,
            status: CheckStatus::Warning,
            message: format!("{name} file not found at {}", path.display()),
        },
        Err(e) => DoctorCheck {
            name,
            status: CheckStatus::Warning,
            message: format!("could not read {name} file at {}: {e}", path.display()),
        },
    }
}

fn state_db_path(username: &str, cookie_dir: &Path, realm: &str) -> Option<PathBuf> {
    let sanitized = crate::account::namespace(username, realm);
    (!username.is_empty()).then(|| cookie_dir.join(format!("{sanitized}.db")))
}

fn detect_install_method() -> &'static str {
    if Path::new("/.dockerenv").exists() || std::env::var_os("KEI_CONTAINER").is_some() {
        return "docker";
    }
    let exe = std::env::current_exe()
        .ok()
        .and_then(|p| p.to_str().map(str::to_owned))
        .unwrap_or_default();
    if exe.contains("/Cellar/") || exe.contains("/Homebrew/") {
        "homebrew"
    } else if exe.contains("/.cargo/bin/") {
        "cargo"
    } else {
        "unknown"
    }
}

fn print_human(report: &DoctorReport) {
    println!("kei doctor");
    println!("  version: {}", report.version);
    println!("  install method: {}", report.install_method);
    println!("  config path: {}", report.config_path);
    println!("  data dir: {}", report.data_dir);
    println!(
        "  report path: {}",
        report.report_path.as_deref().unwrap_or("<not configured>")
    );
    println!();
    println!("Checks:");
    for check in &report.checks {
        println!(
            "  {:<14} {:<7} {}",
            check.name,
            format!("{:?}", check.status).to_ascii_lowercase(),
            check.message
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redaction_removes_known_secrets_and_inline_tokens() {
        let mut redactor = Redactor::default();
        redactor.add("user@example.com");
        redactor.add("secret-password");
        let text = "user@example.com password=secret-password session_token=abc123 cookie:jar-value\nAuthorization: Bearer header-token";
        let redacted = redactor.redact(text);

        assert!(!redacted.contains("user@example.com"));
        assert!(!redacted.contains("secret-password"));
        assert!(!redacted.contains("abc123"));
        assert!(!redacted.contains("jar-value"));
        assert!(!redacted.contains("header-token"));
        assert!(redacted.contains('\n'));
        assert!(redacted.contains("<redacted>"));
    }

    #[test]
    fn redaction_handles_invalid_config_snippets_without_parsed_toml() {
        let redactor = Redactor::default();
        let text = "TOML parse error\n4 | username = \"user@example.com\"\n5 | password = \"secret\"\n6 | webhook_url = \"https://hooks.example/token\"";
        let redacted = redactor.redact(text);

        assert!(!redacted.contains("user@example.com"));
        assert!(!redacted.contains("secret"));
        assert!(!redacted.contains("hooks.example"));
        assert!(redacted.matches("<redacted>").count() >= 3);
    }

    #[test]
    fn json_report_redaction_removes_secrets() {
        let mut redactor = Redactor::default();
        redactor.add("user@example.com");
        let report = DoctorReport {
            version: "test",
            install_method: "unknown",
            config_path: "/tmp/user@example.com/config.toml".to_string(),
            data_dir: "/tmp/user@example.com/data".to_string(),
            report_path: Some("/tmp/user@example.com/report.json".to_string()),
            checks: vec![DoctorCheck {
                name: "session",
                status: CheckStatus::Ok,
                message: "session_token=abc123 for user@example.com".to_string(),
            }],
        };

        let report = redactor.redact_report(report);
        let json = serde_json::to_string(&report).expect("serialize report");

        assert!(!json.contains("user@example.com"));
        assert!(!json.contains("abc123"));
        assert!(json.contains("<redacted>"));
    }

    #[tokio::test]
    async fn state_db_check_reads_real_local_db_path() {
        let dir = tempfile::tempdir().expect("temp dir");
        let username = "doctor@example.com";
        let path = state_db_path(username, dir.path(), "com").expect("state db path");
        let _db = state::SqliteStateDb::open_owned(
            &path,
            &state::db::account::AccountOwner::authenticated(
                username,
                "com",
                &serde_json::from_value(serde_json::json!({"dsInfo":{"dsid":"synthetic-doctor"}}))
                    .unwrap(),
            )
            .unwrap(),
        )
        .await
        .expect("create state db");

        let check = check_state_db(username, dir.path(), "com").await;

        assert_eq!(check.name, "state_db");
        assert_eq!(check.status, CheckStatus::Ok);
        assert!(check.message.contains("0 assets"));
    }

    #[tokio::test]
    async fn live_session_check_skips_without_username() {
        let dir = tempfile::tempdir().expect("temp dir");
        let check = check_live_session("", "com", dir.path()).await;

        assert_eq!(check.name, "live_session");
        assert_eq!(check.status, CheckStatus::Skipped);
        assert!(check.message.contains("no username configured"));
    }
}
