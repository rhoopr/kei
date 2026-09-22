//! Offline branch-coherence checks for packaging and migration docs.
//!
//! These intentionally inspect repository files instead of spawning Docker or
//! contacting iCloud. The live shell suites still own runtime behavior; this
//! file pins the risky static contracts that made this branch easy to regress.

#![allow(clippy::panic, clippy::unwrap_used)]

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

#[cfg(target_os = "linux")]
use std::os::unix::fs::PermissionsExt;
#[cfg(target_os = "linux")]
use std::process::{Command, Output};

fn repo_file(path: &str) -> String {
    let mut full = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    full.push(path);
    std::fs::read_to_string(&full)
        .unwrap_or_else(|e| panic!("read {}: {e}", full.display()))
        .replace("\r\n", "\n")
        .replace('\r', "\n")
}

fn repo_path(path: &str) -> PathBuf {
    let mut full = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    full.push(path);
    full
}

fn missing_required_fragments(contents: &str, required: &[&str]) -> Vec<String> {
    required
        .iter()
        .filter(|fragment| !contents.contains(**fragment))
        .map(|fragment| (*fragment).to_owned())
        .collect()
}

fn normalize_whitespace(contents: &str) -> String {
    contents.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(target_os = "linux")]
fn write_executable(path: &Path, contents: &str) {
    std::fs::write(path, contents).unwrap_or_else(|e| panic!("write {}: {e}", path.display()));
    let mut permissions = std::fs::metadata(path)
        .unwrap_or_else(|e| panic!("stat {}: {e}", path.display()))
        .permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(path, permissions)
        .unwrap_or_else(|e| panic!("chmod {}: {e}", path.display()));
}

#[cfg(target_os = "linux")]
fn command_text(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[cfg(target_os = "linux")]
fn run_git(repo: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(repo)
        .output()
        .unwrap_or_else(|e| panic!("run git {}: {e}", args.join(" ")));
    assert!(
        output.status.success(),
        "git {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );
    command_text(&output).trim().to_owned()
}

#[cfg(target_os = "linux")]
fn assert_text_order(output: &str, names: &[&str]) {
    let mut previous = None;
    for name in names {
        let position = output
            .find(name)
            .unwrap_or_else(|| panic!("output missing {name}:\n{output}"));
        if let Some((previous_name, previous_position)) = previous {
            assert!(
                previous_position < position,
                "expected {previous_name} before {name}:\n{output}"
            );
        }
        previous = Some((name, position));
    }
}

#[cfg(target_os = "linux")]
fn write_json(path: &Path, value: &serde_json::Value) {
    std::fs::write(
        path,
        serde_json::to_vec_pretty(value)
            .unwrap_or_else(|e| panic!("serialize fixture for {}: {e}", path.display())),
    )
    .unwrap_or_else(|e| panic!("write {}: {e}", path.display()));
}

fn rust_files_under(path: &Path, out: &mut Vec<PathBuf>) {
    for entry in
        std::fs::read_dir(path).unwrap_or_else(|e| panic!("read dir {}: {e}", path.display()))
    {
        let entry = entry.unwrap_or_else(|e| panic!("read dir entry {}: {e}", path.display()));
        let path = entry.path();
        if path.is_dir() {
            rust_files_under(&path, out);
        } else if path.extension().and_then(|ext| ext.to_str()) == Some("rs") {
            out.push(path);
        }
    }
}

fn production_source(source: &str) -> &str {
    let source = source
        .split_once("\n#[cfg(test)]\nmod tests")
        .map_or(source, |(prod, _)| prod);
    source
        .split_once("\n#[cfg(test)]\nmod wiremock_tests")
        .map_or(source, |(prod, _)| prod)
}

fn function_name(line: &str) -> Option<String> {
    let line = line.trim_start();
    let (_, rest) = line.split_once("fn ")?;
    let name: String = rest
        .chars()
        .take_while(|ch| ch.is_ascii_alphanumeric() || *ch == '_')
        .collect();
    (!name.is_empty()).then_some(name)
}

#[test]
fn docker_packaging_defaults_to_service_run() {
    let dockerfile = repo_file("Dockerfile");
    assert!(
        dockerfile.contains("ENV KEI_DATA_DIR=/config"),
        "Docker image must keep session state and config under /config"
    );
    assert!(
        dockerfile.contains(r#"CMD ["service", "run", "--config", "/config/config.toml"]"#),
        "Docker default command must run service mode so the container stays alive"
    );
    assert!(
        !dockerfile.contains("KEI_WATCH_WITH_INTERVAL"),
        "removed watch env mirror must not return in the Dockerfile"
    );

    let entrypoint = repo_file("docker/entrypoint.sh");
    let whitelist = entrypoint
        .lines()
        .find(|line| {
            ["sync", "login", "list", "password"]
                .iter()
                .all(|subcommand| line.contains(subcommand))
        })
        .expect("entrypoint must keep an explicit kei subcommand whitelist");
    for subcommand in [
        "sync",
        "login",
        "list",
        "password",
        "reset",
        "config",
        "status",
        "doctor",
        "manifest",
        "verify",
        "reconcile",
        "import-existing",
        "install",
        "uninstall",
        "service",
        "help",
    ] {
        assert!(
            whitelist
                .split(['|', ')', ' ', '\t'])
                .any(|token| token == subcommand),
            "entrypoint whitelist must include the kei `{subcommand}` subcommand"
        );
    }
    let whitelist_index = entrypoint
        .find(whitelist)
        .expect("entrypoint must contain its parsed subcommand whitelist");
    let command_lookup = entrypoint
        .find("command -v")
        .expect("entrypoint should still fall back to command lookup");
    assert!(
        whitelist_index < command_lookup,
        "`service` must be recognized as a kei subcommand before shell command lookup"
    );
}

#[test]
fn docker_runtime_is_version_pinned_and_maintained() {
    let dockerfile = repo_file("Dockerfile");
    let snapshot = dockerfile
        .lines()
        .find_map(|line| line.strip_prefix("FROM debian:bookworm-"))
        .and_then(|suffix| suffix.strip_suffix("-slim"))
        .expect("Docker runtime must use a dated Debian Bookworm slim image");
    assert!(
        snapshot.len() == 8 && snapshot.bytes().all(|byte| byte.is_ascii_digit()),
        "Docker runtime snapshot must use a YYYYMMDD date"
    );

    let dependabot = repo_file(".github/dependabot.yml");
    for expected in [
        "package-ecosystem: docker",
        "interval: weekly",
        "dependency-name: debian",
    ] {
        assert!(
            dependabot.contains(expected),
            "Dependabot must keep the pinned Debian runtime current: missing {expected}"
        );
    }
}

#[test]
fn migration_guide_uses_toml_for_durable_sync_settings() {
    let guide = repo_file("docs/migration-from-icloudpd.md");
    let stale = [
        "kei sync --library",
        "kei sync --download-dir",
        "kei sync --album",
        "| `-p`, `--password` | Same",
        "| `-a`, `--album` | Same",
        "| `--watch-with-interval` | Same",
        "| `--notification-script` | Same flag",
        "| `--threads-num` | `--threads`",
        "| `--report-json`",
        "| `--http-bind`, `--http-port`",
    ];
    for needle in stale {
        assert!(
            !guide.contains(needle),
            "migration guide still advertises stale sync config surface: {needle}"
        );
    }

    for expected in [
        "kei sync --config ~/.config/kei/config.toml",
        "[download]\ndirectory = \"~/Photos/iCloud\"",
        "[filters]\nlibraries = [\"all\"]",
        "[watch].interval",
        "[notifications].script",
        "[download.retry].per_transfer",
        "don't auto-copy files from the old `icloudpd-rs` paths",
        "cp ~/.config/icloudpd-rs/config.toml ~/.config/kei/config.toml",
        "cp ~/.icloudpd-rs/* ~/.config/kei/cookies/",
    ] {
        assert!(
            guide.contains(expected),
            "migration guide missing TOML-first replacement: {expected}"
        );
    }
}

#[test]
fn notification_script_docs_pin_legacy_env_plus_report_json() {
    let changelog = repo_file("CHANGELOG.md");
    assert!(
        changelog.contains(
            "Notification scripts keep the existing `KEI_ICLOUD_USERNAME` and per-cycle `KEI_*` stat variables"
        ),
        "changelog must pin the legacy notification-script env contract"
    );
    assert!(
        changelog.contains("now also receive `KEI_REPORT_JSON` when `[report].json` is configured"),
        "changelog must call out report JSON as an addition"
    );

    let guide = repo_file("docs/migration-from-icloudpd.md");
    assert!(
        guide.contains("kei sends `KEI_EVENT`, `KEI_MESSAGE`, `KEI_ICLOUD_USERNAME`"),
        "migration guide must keep the legacy notification-script username env var"
    );
    assert!(
        guide.contains(
            "per-cycle `KEI_*` stats, and `KEI_REPORT_JSON` when `[report].json` is configured"
        ),
        "migration guide must document legacy stats plus report JSON"
    );

    let example_config = repo_file("example.config.toml");
    assert!(
        example_config.contains(
            "receives KEI_EVENT, KEI_MESSAGE, KEI_ICLOUD_USERNAME, per-cycle KEI_* stats, and KEI_REPORT_JSON when [report].json is configured"
        ),
        "example config must describe the current notification-script env surface"
    );
}

#[test]
fn full_test_routes_child_tempdirs_to_tmp_codex() {
    let run_all = repo_file("scripts/full-test/run_all.sh");
    let tmp_assignment = "full_tmp_dir=\"${KEI_FULL_TEST_TMPDIR:-/tmp/codex/kei/full-test/tmp}\"";
    let tmp_export = "export TMPDIR=\"$full_tmp_dir\"";
    let temp_export = "export TEMP=\"$full_tmp_dir\"";
    let tmp_windows_export = "export TMP=\"$full_tmp_dir\"";
    let shell_scratch_export =
        "export KEI_TEST_SCRATCH_DIR=\"${KEI_TEST_SCRATCH_DIR:-$full_tmp_dir/shell}\"";

    for expected in [
        tmp_assignment,
        "mkdir -p \"$full_tmp_dir\"",
        tmp_export,
        temp_export,
        tmp_windows_export,
        shell_scratch_export,
        "mkdir -p \"$KEI_TEST_SCRATCH_DIR\"",
    ] {
        assert!(
            run_all.contains(expected),
            "full-test orchestrator missing /tmp/codex tempdir setup: {expected}"
        );
    }

    let export_pos = run_all
        .find(tmp_export)
        .expect("full-test must export TMPDIR before live tests");
    let shell_export_pos = run_all
        .find(shell_scratch_export)
        .expect("full-test must export KEI_TEST_SCRATCH_DIR before shell tests");
    let live_pos = run_all
        .find("run_live_phase live_provider")
        .expect("full-test live provider phase must still exist");
    let shell_pos = run_all
        .find("run_shell_suites.sh")
        .expect("full-test shell phase must still exist");

    assert!(
        export_pos < live_pos && export_pos < shell_pos && shell_export_pos < shell_pos,
        "TMPDIR and KEI_TEST_SCRATCH_DIR must be set before live cargo and shell phases allocate tempdirs"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn full_test_release_artifacts_follow_cargo_target_dir() {
    let repo = repo_path("");
    let helper = repo_path("scripts/full-test/cargo_target_dir.sh");

    let default = Command::new(&helper)
        .current_dir("/")
        .env_remove("CARGO_TARGET_DIR")
        .output()
        .expect("run cargo target directory helper without override");
    assert!(default.status.success());
    assert_eq!(
        command_text(&default).trim(),
        repo.join("target").display().to_string()
    );

    let relative = Command::new(&helper)
        .current_dir("/")
        .env("CARGO_TARGET_DIR", "target/full-test")
        .output()
        .expect("run cargo target directory helper with relative override");
    assert!(relative.status.success());
    assert_eq!(
        command_text(&relative).trim(),
        repo.join("target/full-test").display().to_string()
    );

    let absolute_dir = repo.join("target/full-test-absolute");
    let absolute = Command::new(&helper)
        .current_dir("/")
        .env("CARGO_TARGET_DIR", &absolute_dir)
        .output()
        .expect("run cargo target directory helper with absolute override");
    assert!(absolute.status.success());
    assert_eq!(
        command_text(&absolute).trim(),
        absolute_dir.display().to_string()
    );

    for consumer in [
        "scripts/full-test/run_release_archive_smoke.sh",
        "scripts/full-test/run_live_smokes.sh",
        "tests/shell/lib.sh",
        "justfile",
    ] {
        assert!(
            repo_file(consumer).contains("cargo_target_dir.sh"),
            "{consumer} must resolve release artifacts through cargo_target_dir.sh"
        );
    }
}

#[test]
fn full_test_run_start_metadata_is_stable_until_finalize() {
    let begin = repo_file("scripts/full-test/begin_run.sh");
    let finalize = repo_file("scripts/full-test/finalize_run.sh");

    for expected in [
        "start_file=\"$runs_dir/.run-started-at\"",
        "start_head_file=\"$runs_dir/.run-start-head\"",
        "start_worktree_file=\"$runs_dir/.run-start-worktree-clean\"",
        "lockfile=\"$runs_dir/.lock\"",
        "flock 9",
        "if [[ $marker_age -lt 3600 ]]; then",
        "staging: $current (no records yet)",
        "date +%Y-%m-%dT%H:%M:%S >\"$start_file\"",
        "git rev-parse HEAD >\"$start_head_file\"",
        "git status --porcelain=v1 --untracked-files=all",
    ] {
        assert!(
            begin.contains(expected),
            "begin_run must atomically record stable run-start metadata: {expected}"
        );
    }

    for expected in [
        "start_file=\"$runs_dir/.run-started-at\"",
        "start_head_file=\"$runs_dir/.run-start-head\"",
        "start_worktree_file=\"$runs_dir/.run-start-worktree-clean\"",
        "if [[ -s \"$start_file\" ]]; then",
        "started_at=$(head -n 1 \"$start_file\")",
        "head=$(head -n 1 \"$start_head_file\")",
        "end_head=$(git rev-parse HEAD",
        "start_worktree_clean=$(head -n 1 \"$start_worktree_file\")",
        "end_worktree_clean=true",
        "\"end_head\": end_head",
        "\"start_worktree_clean\": start_clean == \"true\"",
        "\"end_worktree_clean\": end_clean == \"true\"",
        "rm -f \"$current\" \"$runs_dir/.run-marker\" \"$start_file\" \"$start_head_file\" \"$start_worktree_file\"",
    ] {
        assert!(
            finalize.contains(expected),
            "finalize_run must use and clean the stable run-start metadata: {expected}"
        );
    }
}

#[test]
fn full_test_reports_include_newer_phase_metadata() {
    let render = repo_file("scripts/full-test/render_summary.py");
    let diff = repo_file("scripts/full-test/diff_runs.sh");

    for phase in [
        "static_checks",
        "offline_core",
        "scenarios",
        "nightly_tools",
        "package",
        "docker_full",
        "live_provider",
        "live_import_rehearsal",
        "service",
        "host_service",
    ] {
        assert!(
            render.contains(phase),
            "render_summary.py must sort and display newer full-test phase {phase}"
        );
        assert!(
            diff.contains(phase),
            "diff_runs.sh must assign phase number/test metadata for {phase}"
        );
    }
}

#[test]
fn focused_scenario_catalog_lists_every_runner_slice() {
    let readme = repo_file("tests/README.md");
    let section = readme
        .split_once("## Focused scenario slices")
        .map(|(_, tail)| tail)
        .and_then(|tail| tail.split_once("\n## ").map(|(section, _)| section))
        .expect("tests README must contain a bounded focused scenario section");
    let documented: BTreeSet<String> = section
        .lines()
        .filter_map(|line| {
            line.strip_prefix("| `")
                .and_then(|tail| tail.split_once("` |"))
                .map(|(name, _)| name.to_owned())
        })
        .collect();

    let scenario_dir = repo_path("scripts/test-scenarios");
    let scripts: BTreeSet<String> = std::fs::read_dir(&scenario_dir)
        .unwrap_or_else(|e| panic!("read {}: {e}", scenario_dir.display()))
        .map(|entry| {
            entry.unwrap_or_else(|e| panic!("read entry in {}: {e}", scenario_dir.display()))
        })
        .filter_map(|entry| {
            let path = entry.path();
            if path.extension().and_then(|extension| extension.to_str()) != Some("sh") {
                return None;
            }
            let name = path.file_stem()?.to_str()?;
            (!matches!(name, "lib" | "list")).then(|| name.to_owned())
        })
        .collect();

    assert_eq!(
        documented, scripts,
        "tests README focused scenario catalog must match runnable scenario scripts"
    );
}

#[test]
fn config_reconciliation_scenario_pins_transition_seed_tests() {
    let scenario = repo_file("scripts/test-scenarios/config-reconciliation.sh");
    let required = [
        "path_reconciliation_copies_catalog_file_without_provider_inventory",
        "local_reconciliation_copy_preserves_source_and_refuses_conflict",
        "download_config_hash_drift_stages_reconciliation_without_clearing_token",
        "download_config_revert_clears_pending_reconciliation",
        "run_cycle_date_bound_expansion_preserves_existing_media_and_reaches_steady_state",
        "run_cycle_download_config_hash_drift_keeps_source_incremental",
        "run_cycle_multi_pass_persists_base_download_config_hash",
        "unchanged_multi_pass_second_cycle_is_not_download_config_hash_drift",
        "run_cycle_capture_offset_drives_date_filter_path_and_sidecar",
    ];

    let missing = missing_required_fragments(&scenario, &required);
    assert!(
        missing.is_empty(),
        "config-reconciliation scenario missing seed tests: {missing:?}"
    );

    let deliberately_broken = scenario.replacen(required[0], "removed_seed_test", 1);
    assert_eq!(
        missing_required_fragments(&deliberately_broken, &required),
        vec![required[0].to_owned()],
        "the scenario contract check must detect a deliberately removed seed test"
    );
}

#[test]
fn state_transition_proof_is_pinned_across_process_surfaces() {
    let applicability = "Changes to durable configuration, filesystem paths, media publication, metadata, SQLite state, retry work, or provider checkpoints require a state-transition proof through the production call graph.";
    let stages = [
        "Initial durable state",
        "Controlled mutation",
        "Production cycle",
        "Durable outcome",
        "Steady-state cycle",
    ];
    for path in [
        "CONTRIBUTING.md",
        "tests/README.md",
        ".github/pull_request_template.md",
        ".agents/skills/kei-pr-ready/SKILL.md",
    ] {
        let contents = repo_file(path);
        let normalized = normalize_whitespace(&contents);
        assert!(
            normalized.contains(applicability),
            "{path} state-transition applicability categories drifted"
        );
        let missing = missing_required_fragments(&contents, &stages);
        assert!(
            missing.is_empty(),
            "{path} missing state-transition proof stages: {missing:?}"
        );
    }

    let deliberately_drifted = repo_file("CONTRIBUTING.md").replacen("retry work, ", "", 1);
    assert!(
        !normalize_whitespace(&deliberately_drifted).contains(applicability),
        "the process contract check must detect a deliberately removed applicability category"
    );

    let readiness = repo_file(".agents/skills/kei-pr-ready/SKILL.md");
    let readiness_requirements = [
        "alternate byte-landing and",
        "downloaded-state finalization route",
        "correctness, safety, liveness, performance, and user-visible metadata",
        "A normal-download test does not",
        "Do not label a complete owner or module \"fully inspected\"",
    ];
    let missing = missing_required_fragments(&readiness, &readiness_requirements);
    assert!(
        missing.is_empty(),
        "kei-pr-ready missing cross-route review requirements: {missing:?}"
    );

    let template = repo_file(".github/pull_request_template.md");
    let template_requirements = ["Deliberate defect mutation", "Not applicable"];
    let missing = missing_required_fragments(&template, &template_requirements);
    assert!(
        missing.is_empty(),
        "pull request template missing transition evidence fields: {missing:?}"
    );

    let deliberately_broken = template.replacen(stages[4], "Repeat run", 1);
    assert_eq!(
        missing_required_fragments(&deliberately_broken, &stages),
        vec![stages[4].to_owned()],
        "the process contract check must detect a deliberately removed transition stage"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn scenario_runner_rejects_filters_that_match_no_tests() {
    let temp = tempfile::tempdir().expect("scenario runner tempdir");
    let cargo_stub = temp.path().join("cargo-stub");
    write_executable(
        &cargo_stub,
        r#"#!/usr/bin/env bash
set -euo pipefail
if [[ " $* " == *" --list "* && " $* " == *" known_filter "* ]]; then
  echo "module::known_filter: test"
fi
"#,
    );

    let runner = repo_path("scripts/test-scenarios/lib.sh");
    let run_filter = |filter: &str| {
        Command::new("bash")
            .args([
                "-c",
                r#"source "$1"; run_scenario_test lib "$2""#,
                "scenario-runner-test",
            ])
            .arg(&runner)
            .arg(filter)
            .env("CARGO", &cargo_stub)
            .output()
            .expect("run scenario helper")
    };

    let known = run_filter("known_filter");
    assert!(
        known.status.success(),
        "known scenario filter should execute: {}",
        String::from_utf8_lossy(&known.stderr)
    );

    let missing = run_filter("missing_filter");
    assert_eq!(missing.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&missing.stderr);
    assert!(
        stderr.contains("no tests matched target=lib filter=missing_filter"),
        "zero-match failure should identify the target and filter: {stderr}"
    );

    let list = Command::new("bash")
        .arg(repo_path("scripts/test-scenarios/list.sh"))
        .output()
        .expect("list scenarios");
    assert!(list.status.success());
    let listed = command_text(&list);
    assert!(listed.lines().any(|line| line == "pending-recovery"));
    assert!(!listed.lines().any(|line| line == "lib"));
}

#[cfg(target_os = "linux")]
#[test]
fn full_test_reporting_executes_grouped_and_legacy_fixtures() {
    let temp = tempfile::tempdir().expect("report fixture tempdir");
    let grouped_phases = serde_json::json!({
        "service": {"status": "pass", "wall_s": 1.0},
        "scenarios": {"status": "pass", "wall_s": 2.0, "tests": 3},
        "offline_core": {"status": "pass", "wall_s": 3.0, "tests": 4},
        "static_checks": {"status": "pass", "wall_s": 4.0}
    });
    let legacy_phases = serde_json::json!({
        "service_smoke": {"status": "pass", "wall_s": 1.0},
        "offline_all": {"status": "pass", "wall_s": 2.0, "tests": 5},
        "nodefault": {"status": "pass", "wall_s": 3.0},
        "gate": {"status": "pass", "wall_s": 4.0}
    });

    let grouped_record = temp.path().join("grouped.json");
    let legacy_record = temp.path().join("legacy.json");
    write_json(
        &grouped_record,
        &serde_json::json!({"phases": grouped_phases.clone(), "metrics": {}}),
    );
    write_json(
        &legacy_record,
        &serde_json::json!({"phases": legacy_phases.clone(), "metrics": {}}),
    );

    let render = repo_path("scripts/full-test/render_summary.py");
    let grouped_render = Command::new("python3")
        .arg(&render)
        .arg(&grouped_record)
        .args(["--result", "pass"])
        .output()
        .expect("render grouped fixture");
    assert!(grouped_render.status.success());
    assert_text_order(
        &command_text(&grouped_render),
        &["static_checks", "offline_core", "scenarios", "service"],
    );

    let legacy_render = Command::new("python3")
        .arg(&render)
        .arg(&legacy_record)
        .args(["--result", "pass"])
        .output()
        .expect("render legacy fixture");
    assert!(legacy_render.status.success());
    assert_text_order(
        &command_text(&legacy_render),
        &["gate", "nodefault", "offline_all", "service_smoke"],
    );

    let diff = repo_path("scripts/full-test/diff_runs.sh");
    for (name, phases, order) in [
        (
            "grouped",
            grouped_phases,
            vec!["static_checks", "offline_core", "scenarios", "service"],
        ),
        (
            "legacy",
            legacy_phases,
            vec!["gate", "nodefault", "offline_all", "service_smoke"],
        ),
    ] {
        let runs = temp.path().join(name);
        std::fs::create_dir(&runs).expect("create report runs dir");
        for (timestamp, head) in [("20260716T000000", "old"), ("20260717T000000", "new")] {
            write_json(
                &runs.join(format!("{timestamp}.json")),
                &serde_json::json!({
                    "started_at": timestamp,
                    "branch": "fixture",
                    "head": head,
                    "phases": phases.clone(),
                    "metrics": {}
                }),
            );
        }
        let output = Command::new("bash")
            .arg(&diff)
            .env("KEI_FULL_TEST_RUNS_DIR", &runs)
            .output()
            .expect("diff run fixtures");
        assert!(
            output.status.success(),
            "diff fixture failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_text_order(&command_text(&output), &order);
    }
}

#[cfg(target_os = "linux")]
#[test]
fn full_test_finalize_emits_metrics_and_cleans_staging() {
    let temp = tempfile::tempdir().expect("finalize fixture tempdir");
    let repo = temp.path().join("repo");
    let runs = temp.path().join("runs");
    let bin = temp.path().join("bin");
    std::fs::create_dir(&repo).expect("create fixture repo");
    std::fs::create_dir(&runs).expect("create finalize runs dir");
    std::fs::create_dir(&bin).expect("create stub bin dir");

    run_git(&repo, &["init", "-b", "fixture"]);
    run_git(&repo, &["config", "user.name", "Kei Test"]);
    run_git(&repo, &["config", "user.email", "kei-test@example.invalid"]);
    std::fs::write(repo.join("tracked.txt"), "fixture\n").expect("write fixture file");
    std::fs::write(repo.join("Cargo.lock"), "name = \"fixture\"\n")
        .expect("write fixture lockfile");
    run_git(&repo, &["add", "tracked.txt", "Cargo.lock"]);
    run_git(&repo, &["commit", "-m", "fixture"]);

    std::fs::write(
        runs.join(".current.jsonl"),
        "{\"phase\":\"static_checks\",\"status\":\"pass\",\"wall_s\":1.25,\"tests\":3}\n",
    )
    .expect("write phase fixture");
    std::fs::write(runs.join(".run-started-at"), "2026-07-17T12:34:56\n")
        .expect("write start fixture");
    let head = run_git(&repo, &["rev-parse", "HEAD"]);
    std::fs::write(runs.join(".run-start-head"), format!("{head}\n"))
        .expect("write start head fixture");
    std::fs::write(runs.join(".run-start-worktree-clean"), "true\n")
        .expect("write start worktree fixture");
    std::fs::write(runs.join(".run-marker"), "fixture\n").expect("write marker fixture");

    write_executable(&bin.join("cargo"), "#!/usr/bin/env bash\nexit 0\n");
    write_executable(&bin.join("docker"), "#!/usr/bin/env bash\nexit 1\n");
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").expect("PATH must be set")
    );

    let output = Command::new("bash")
        .arg(repo_path("scripts/full-test/finalize_run.sh"))
        .current_dir(&repo)
        .env("KEI_FULL_TEST_RUNS_DIR", &runs)
        .env("PATH", path)
        .output()
        .expect("finalize fixture run");
    assert!(
        output.status.success(),
        "finalize fixture failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let record_path = PathBuf::from(command_text(&output).trim());
    let record: serde_json::Value = serde_json::from_slice(
        &std::fs::read(&record_path)
            .unwrap_or_else(|e| panic!("read {}: {e}", record_path.display())),
    )
    .expect("parse finalized record");
    assert_eq!(record["started_at"], "2026-07-17T12:34:56");
    assert_eq!(record["head"], head);
    assert_eq!(record["end_head"], head);
    assert_eq!(record["start_worktree_clean"], true);
    assert_eq!(record["end_worktree_clean"], true);
    assert_eq!(record["phases"]["static_checks"]["status"], "pass");
    assert_eq!(record["phases"]["static_checks"]["tests"], 3);
    assert!(record["metrics"].is_object());
    assert!(record["metrics"]["deps_count"].is_number());
    for staging in [
        ".current.jsonl",
        ".run-started-at",
        ".run-start-head",
        ".run-start-worktree-clean",
        ".run-marker",
    ] {
        assert!(
            !runs.join(staging).exists(),
            "finalize must remove {staging}"
        );
    }
}

#[cfg(target_os = "linux")]
#[test]
fn full_test_head_change_is_not_current_validation() {
    let temp = tempfile::tempdir().expect("head change fixture tempdir");
    let repo = temp.path().join("repo");
    let runs = temp.path().join("runs");
    let bin = temp.path().join("bin");
    std::fs::create_dir_all(&repo).expect("create fixture repo");
    std::fs::create_dir_all(&runs).expect("create fixture runs directory");
    std::fs::create_dir_all(&bin).expect("create stub bin directory");

    run_git(&repo, &["init", "-b", "feature"]);
    run_git(&repo, &["config", "user.name", "Kei Test"]);
    run_git(&repo, &["config", "user.email", "kei-test@example.invalid"]);
    std::fs::write(repo.join("tracked.txt"), "start\n").expect("write starting file");
    run_git(&repo, &["add", "tracked.txt"]);
    run_git(&repo, &["commit", "-m", "start"]);
    let start_head = run_git(&repo, &["rev-parse", "HEAD"]);

    let begin = Command::new(repo_path("scripts/full-test/begin_run.sh"))
        .current_dir(&repo)
        .env("KEI_FULL_TEST_RUNS_DIR", &runs)
        .output()
        .expect("begin full-test fixture run");
    assert!(
        begin.status.success(),
        "begin fixture failed: {}",
        String::from_utf8_lossy(&begin.stderr)
    );
    std::fs::write(
        runs.join(".current.jsonl"),
        "{\"phase\":\"static_checks\",\"status\":\"pass\",\"wall_s\":1.0}\n",
    )
    .expect("write phase fixture");

    std::fs::write(repo.join("tracked.txt"), "end\n").expect("write ending file");
    run_git(&repo, &["add", "tracked.txt"]);
    run_git(&repo, &["commit", "-m", "end"]);
    let end_head = run_git(&repo, &["rev-parse", "HEAD"]);

    write_executable(&bin.join("cargo"), "#!/usr/bin/env bash\nexit 0\n");
    write_executable(&bin.join("docker"), "#!/usr/bin/env bash\nexit 1\n");
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").expect("PATH must be set")
    );
    let finalize = Command::new(repo_path("scripts/full-test/finalize_run.sh"))
        .current_dir(&repo)
        .env("KEI_FULL_TEST_RUNS_DIR", &runs)
        .env("PATH", path)
        .output()
        .expect("finalize full-test fixture run");
    assert!(
        finalize.status.success(),
        "finalize fixture failed: {}",
        String::from_utf8_lossy(&finalize.stderr)
    );

    let record_path = PathBuf::from(command_text(&finalize).trim());
    let record: serde_json::Value = serde_json::from_slice(
        &std::fs::read(&record_path)
            .unwrap_or_else(|e| panic!("read {}: {e}", record_path.display())),
    )
    .expect("parse head change record");
    assert_eq!(record["head"], start_head);
    assert_eq!(record["end_head"], end_head);
    assert_eq!(record["start_worktree_clean"], true);
    assert_eq!(record["end_worktree_clean"], true);

    let scope = Command::new(repo_path("scripts/just/review-scope.sh"))
        .arg("--validation-only")
        .current_dir(&repo)
        .env("KEI_FULL_TEST_RUNS_DIR", &runs)
        .output()
        .expect("check changed-head validation provenance");
    assert!(scope.status.success());
    let scope = command_text(&scope);
    assert!(scope.contains(&format!("validation_head: {start_head}")));
    assert!(scope.contains(&format!("validation_end_head: {end_head}")));
    assert!(
        scope.contains("validation_status: STALE"),
        "a run that changed heads must not be current:\n{scope}"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn full_test_dirty_start_is_not_current_validation() {
    let temp = tempfile::tempdir().expect("dirty start fixture tempdir");
    let repo = temp.path().join("repo");
    let runs = temp.path().join("runs");
    let bin = temp.path().join("bin");
    std::fs::create_dir_all(&repo).expect("create fixture repo");
    std::fs::create_dir_all(&runs).expect("create fixture runs directory");
    std::fs::create_dir_all(&bin).expect("create stub bin directory");

    run_git(&repo, &["init", "-b", "feature"]);
    run_git(&repo, &["config", "user.name", "Kei Test"]);
    run_git(&repo, &["config", "user.email", "kei-test@example.invalid"]);
    std::fs::write(repo.join("tracked.txt"), "committed\n").expect("write committed file");
    run_git(&repo, &["add", "tracked.txt"]);
    run_git(&repo, &["commit", "-m", "base"]);
    let head = run_git(&repo, &["rev-parse", "HEAD"]);
    std::fs::write(repo.join("tracked.txt"), "validated dirty bytes\n")
        .expect("write dirty fixture");

    let begin = Command::new(repo_path("scripts/full-test/begin_run.sh"))
        .current_dir(&repo)
        .env("KEI_FULL_TEST_RUNS_DIR", &runs)
        .output()
        .expect("begin dirty fixture run");
    assert!(
        begin.status.success(),
        "begin fixture failed: {}",
        String::from_utf8_lossy(&begin.stderr)
    );
    std::fs::write(
        runs.join(".current.jsonl"),
        "{\"phase\":\"static_checks\",\"status\":\"pass\",\"wall_s\":1.0}\n",
    )
    .expect("write phase fixture");
    std::fs::write(repo.join("tracked.txt"), "committed\n").expect("restore committed file");

    write_executable(&bin.join("cargo"), "#!/usr/bin/env bash\nexit 0\n");
    write_executable(&bin.join("docker"), "#!/usr/bin/env bash\nexit 1\n");
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").expect("PATH must be set")
    );
    let finalize = Command::new(repo_path("scripts/full-test/finalize_run.sh"))
        .current_dir(&repo)
        .env("KEI_FULL_TEST_RUNS_DIR", &runs)
        .env("PATH", path)
        .output()
        .expect("finalize dirty fixture run");
    assert!(
        finalize.status.success(),
        "finalize fixture failed: {}",
        String::from_utf8_lossy(&finalize.stderr)
    );

    let record_path = PathBuf::from(command_text(&finalize).trim());
    let record: serde_json::Value = serde_json::from_slice(
        &std::fs::read(&record_path)
            .unwrap_or_else(|e| panic!("read {}: {e}", record_path.display())),
    )
    .expect("parse dirty-start record");
    assert_eq!(record["head"], head);
    assert_eq!(record["end_head"], head);
    assert_eq!(record["start_worktree_clean"], false);
    assert_eq!(record["end_worktree_clean"], true);

    let scope = Command::new(repo_path("scripts/just/review-scope.sh"))
        .arg("--validation-only")
        .current_dir(&repo)
        .env("KEI_FULL_TEST_RUNS_DIR", &runs)
        .output()
        .expect("check dirty-start validation provenance");
    assert!(scope.status.success());
    let scope = command_text(&scope);
    assert!(scope.contains("validation_start_worktree_clean: false"));
    assert!(scope.contains("validation_end_worktree_clean: true"));
    assert!(
        scope.contains("validation_status: STALE"),
        "a run that started dirty must not be current:\n{scope}"
    );
}

#[test]
fn scenario_fulltest_harness_rejects_unreferenced_helpers() {
    let full_test_dir = repo_path("scripts/full-test");
    let mut corpus = String::new();
    for path in [
        "justfile",
        "tests/README.md",
        "scripts/full-test/run_all.sh",
    ] {
        corpus.push_str(&repo_file(path));
        corpus.push('\n');
    }
    for entry in std::fs::read_dir(&full_test_dir)
        .unwrap_or_else(|e| panic!("read dir {}: {e}", full_test_dir.display()))
    {
        let entry =
            entry.unwrap_or_else(|e| panic!("read dir entry {}: {e}", full_test_dir.display()));
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .expect("script file name must be utf8");
        if matches!(name, "run_all.sh") {
            continue;
        }
        let mut references = corpus.clone();
        for other in std::fs::read_dir(&full_test_dir)
            .unwrap_or_else(|e| panic!("read dir {}: {e}", full_test_dir.display()))
        {
            let other =
                other.unwrap_or_else(|e| panic!("read dir entry {}: {e}", full_test_dir.display()));
            let other_path = other.path();
            if other_path == path || !other_path.is_file() {
                continue;
            }
            references.push_str(
                &std::fs::read_to_string(&other_path)
                    .unwrap_or_else(|e| panic!("read {}: {e}", other_path.display())),
            );
            references.push('\n');
        }
        assert!(
            references.contains(name),
            "scripts/full-test/{name} is not referenced by the live harness, just recipes, tests, or docs"
        );
    }

    let justfile = repo_file("justfile");
    let run_all = repo_file("scripts/full-test/run_all.sh");
    let finalize = repo_file("scripts/full-test/finalize_run.sh");
    assert!(
        !justfile.contains("backfill_metrics")
            && !run_all.contains("backfill_metrics")
            && !finalize.contains("backfill_metrics"),
        "historical metrics backfill helper must not be part of the current full-test path"
    );
    for expected in [
        r#"metrics_json=$("$script_dir/collect_metrics.py" 2>/dev/null || echo "{}")"#,
        "\"metrics\": json.loads(metrics_json or \"{}\")",
        "phases[phase] = rec",
    ] {
        assert!(
            finalize.contains(expected),
            "finalize_run must keep current metrics generation without a backfill step: {expected}"
        );
    }
}

#[test]
fn full_test_prereqs_report_script_tooling_gaps() {
    let prereqs = repo_file("scripts/full-test/check_prereqs.sh");

    for expected in [
        "optional-missing $cmd not found",
        "report_tool shellcheck shellcheck 1",
        "report_tool shfmt shfmt 1",
        "report_tool ruff ruff 1",
        "report_tool actionlint actionlint 1",
        "report_tool cargo-bloat cargo-bloat 1",
    ] {
        assert!(
            prereqs.contains(expected),
            "full-test prereqs must report script/tooling availability: {expected}"
        );
    }
}

#[test]
fn full_test_checks_gnu_linux_userland_before_begin_run() {
    let run_all = repo_file("scripts/full-test/run_all.sh");
    let check = repo_file("scripts/full-test/check_userland.sh");

    assert!(
        run_all.contains(r#""$script_dir/check_userland.sh""#),
        "full-test must check local userland before mutating run state"
    );
    let check_pos = run_all
        .find(r#""$script_dir/check_userland.sh""#)
        .expect("userland check must be present");
    let begin_pos = run_all
        .find(r#"run_id=$("$script_dir/begin_run.sh")"#)
        .expect("begin_run call must still exist");
    assert!(
        check_pos < begin_pos,
        "userland check must run before begin_run writes markers"
    );

    for expected in [
        "find does not support GNU -printf",
        "stat does not support GNU -c",
        "timeout command is present but failed a basic smoke test",
        "full-test: unsupported local userland",
        "GNU/Linux userland tools",
    ] {
        assert!(
            check.contains(expected),
            "userland check must explain unsupported local tooling: {expected}"
        );
    }
}

#[test]
fn full_test_docker_smokes_quote_configured_image() {
    let run_all = repo_file("scripts/full-test/run_all.sh");
    let justfile = repo_file("justfile");
    let shell_suites = repo_file("scripts/full-test/run_shell_suites.sh");
    let docker_puid = repo_file("scripts/full-test/run_docker_puid_smoke.sh");
    let shell_lib = repo_file("tests/shell/lib.sh");

    assert!(
        run_all.contains(r#"export KEI_DOCKER_IMAGE="${KEI_DOCKER_IMAGE:-kei:dev}""#),
        "full-test must export the configured docker image default"
    );
    assert!(
        run_all.contains("run_phase docker_full -- just test docker-full"),
        "full-test docker group must route through the named docker-full recipe"
    );

    for expected in [
        r#"docker run --rm "${KEI_DOCKER_IMAGE:-kei:dev}" --version"#,
        r#"docker run --rm "${KEI_DOCKER_IMAGE:-kei:dev}" --help"#,
        r#"timeout 8 docker run --rm -e ICLOUD_USERNAME=dummy@example.com "${KEI_DOCKER_IMAGE:-kei:dev}""#,
        "set +e",
        "[[ $rc -ne 2 ]]",
    ] {
        assert!(
            justfile.contains(expected),
            "docker-full recipe must use the configured, quoted docker image: {expected}"
        );
    }

    for expected in [
        r#"image="${KEI_DOCKER_IMAGE:-kei:dev}""#,
        r#"KEI_DOCKER_IMAGE="$image""#,
    ] {
        assert!(
            shell_suites.contains(expected),
            "shell suite runner must pass the selected docker image through: {expected}"
        );
    }
    assert!(
        docker_puid.contains(r#"image="${KEI_DOCKER_IMAGE:-kei:dev}""#),
        "docker PUID smoke must use the same KEI_DOCKER_IMAGE default as full-test"
    );
    assert!(
        shell_lib.contains(r#"printf '%s' "${KEI_DOCKER_IMAGE:-kei:latest}""#),
        "standalone shell tests must keep their documented local default"
    );
}

#[test]
fn local_gate_includes_script_and_workflow_lint_recipes() {
    let justfile = repo_file("justfile");
    let ci = repo_file(".github/workflows/ci.yml");

    for expected in [
        "static-checks:",
        "lint-workflows:",
        "python3 .github/scripts/check_workflow_hardening.py",
        "PYTHONPYCACHEPREFIX=\"$pycache_dir\" python3 -m py_compile .github/scripts/*.py",
        "actionlint .github/workflows/*.yml",
        "lint-scripts:",
        "python_files+=(scripts/check-contracts)",
        "for shell_file in \"${shell_files[@]}\"; do",
        "bash -n \"$shell_file\"",
        "PYTHONPYCACHEPREFIX=\"$pycache_dir\" python3 -m py_compile \"${python_files[@]}\"",
        "shellcheck -x -P tests/shell:scripts:scripts/full-test \"${shell_files[@]}\"",
        "shfmt -d \"${shell_files[@]}\"",
        "ruff check \"${python_files[@]}\"",
    ] {
        assert!(
            justfile.contains(expected),
            "justfile must keep script/workflow lint coverage: {expected}"
        );
    }

    let static_checks = justfile
        .split_once("static-checks:\n")
        .map(|(_, tail)| tail)
        .and_then(|tail| tail.split_once("\n\n").map(|(recipe, _)| recipe))
        .expect("justfile must keep static-checks recipe");
    for expected in ["just lint-workflows", "just lint-scripts"] {
        assert!(
            static_checks.contains(expected),
            "just static-checks must run {expected}"
        );
    }

    let gate = justfile
        .split_once("gate:\n")
        .map(|(_, tail)| tail)
        .and_then(|tail| tail.split_once("\n\n").map(|(gate, _)| gate))
        .expect("justfile must keep gate recipe");
    for expected in [
        "just static-checks",
        "cargo test --all-features",
        "cargo test --no-default-features",
    ] {
        assert!(gate.contains(expected), "just gate must run {expected}");
    }

    assert!(
        ci.contains("  script-lint:\n"),
        "CI workflow must keep the script-lint job"
    );
    assert!(
        ci.contains("PYTHONPYCACHEPREFIX=/tmp/codex/kei/pycache python3 -m py_compile"),
        "CI script lint must route generated Python bytecode outside the repo tree"
    );
    for expected in [
        "jdx/mise-action@5228313ee0372e111a38da051671ca30fc5a96db",
        "actionlint = \"1.7.12\"",
        "ruff = \"0.16.3\"",
        "shellcheck = \"0.11.0\"",
        "shfmt = \"3.13.1\"",
        "python_files+=(scripts/check-contracts)",
        "for shell_file in \"${shell_files[@]}\"; do",
        "bash -n \"$shell_file\"",
        "shellcheck -x -P tests/shell:scripts:scripts/full-test \"${shell_files[@]}\"",
        "shfmt -d \"${shell_files[@]}\"",
        "ruff check \"${python_files[@]}\"",
        "actionlint .github/workflows/*.yml",
    ] {
        assert!(
            ci.contains(expected),
            "CI script lint must check each script with the matching interpreter: {expected}"
        );
    }
    let aggregate = ci
        .split_once("  ci:\n")
        .map(|(_, tail)| tail)
        .expect("CI aggregate job must exist");
    assert!(
        aggregate.contains("      - script-lint\n"),
        "aggregate CI job must require script-lint"
    );
}

#[test]
fn aggregate_ci_depends_on_no_default_feature_gate() {
    let ci = repo_file(".github/workflows/ci.yml");
    assert!(
        ci.contains("  test_no_default:\n"),
        "CI workflow must keep the no-default-features job"
    );

    let aggregate = ci
        .split_once("  ci:\n")
        .map(|(_, tail)| tail)
        .expect("CI aggregate job must exist");
    assert!(
        aggregate.contains("      - test_no_default\n"),
        "aggregate CI job must require test_no_default so branch protection sees no-default failures"
    );
}

#[test]
fn rust_ci_runs_on_main_push_without_pr_only_coverage() {
    let ci = repo_file(".github/workflows/ci.yml");
    let hardening = repo_file(".github/scripts/check_workflow_hardening.py");

    for expected in [
        "  push:\n    branches: [main]",
        "if [[ \"$EVENT_NAME\" != \"pull_request\" ]]; then",
        "git diff --name-only \"${BASE_SHA}\"...HEAD > \"$files_path\"",
        "mapfile -t files < \"$files_path\"",
        "github.event_name == 'pull_request' && needs.detect.outputs.code == 'true'",
    ] {
        assert!(
            ci.contains(expected),
            "CI must run on main pushes while keeping coverage PR-only: {expected}"
        );
    }

    for expected in [
        "push:\\n    branches: [main]",
        "if [[ \"$EVENT_NAME\" != \"pull_request\" ]]; then",
        "github.event_name == 'pull_request' && needs.detect.outputs.code == 'true'",
    ] {
        assert!(
            hardening.contains(expected),
            "workflow hardening must pin the CI push/coverage guard: {expected}"
        );
    }
}

#[test]
fn release_homebrew_downloads_fail_fast_and_verify_checksums() {
    let release = repo_file(".github/workflows/release.yml");
    let hardening = repo_file(".github/scripts/check_workflow_hardening.py");

    for expected in [
        "sha256sum -- *.tar.gz *.zip > SHA256SUMS.txt",
        r#"curl -fsSL "$BASE/SHA256SUMS.txt" -o /tmp/SHA256SUMS.txt"#,
        r#"curl -fsSL "$BASE/$file" -o "/tmp/$file""#,
        r#"expected_sha=$(awk -v file="$file" '$2 == file { print $1 }' /tmp/SHA256SUMS.txt)"#,
        r#"if [ "$actual_sha" != "$expected_sha" ]; then"#,
        r#"SHAS[$key]="$actual_sha""#,
    ] {
        assert!(
            release.contains(expected),
            "release Homebrew update must fail fast and verify checksums: {expected}"
        );
    }

    for expected in [
        r#"curl -fsSL "$BASE/SHA256SUMS.txt""#,
        r#"curl -fsSL "$BASE/$file""#,
        r#"expected_sha=$(awk -v file="$file""#,
        r#"if [ "$actual_sha" != "$expected_sha" ]; then"#,
        r#"SHAS[$key]="$actual_sha""#,
    ] {
        assert!(
            hardening.contains(expected),
            "workflow hardening script must pin release invariant: {expected}"
        );
    }
}

#[test]
fn service_smoke_path_filters_cover_shared_dispatch() {
    let service_smoke = repo_file(".github/workflows/service-smoke.yml");
    let hardening = repo_file(".github/scripts/check_workflow_hardening.py");

    for path in [
        "src/service/**",
        "src/commands/service.rs",
        "src/cli.rs",
        "src/config.rs",
        "src/config/**",
        "src/lib.rs",
        "src/commands/status.rs",
    ] {
        let expected = format!("- '{path}'");
        assert!(
            service_smoke.contains(&expected),
            "service-smoke path filter must include {path}"
        );
        assert!(
            hardening.contains(path),
            "workflow hardening script must enforce service-smoke path filter for {path}"
        );
    }
}

#[test]
fn contributor_docs_match_current_gate() {
    let contributing = repo_file("CONTRIBUTING.md");
    let pr_template = repo_file(".github/pull_request_template.md");

    for expected in [
        "cargo fmt --all --check",
        "cargo clippy --all-targets --all-features -- -D warnings",
        "cargo clippy --all-targets --no-default-features -- -D warnings",
        "cargo test --all-features",
        "cargo test --no-default-features",
        "RUSTDOCFLAGS=\"-Dwarnings\" cargo doc --no-deps --all-features",
        "cargo audit --deny warnings",
        "python3 .github/scripts/check_workflow_hardening.py",
        "python_files+=(scripts/check-contracts)",
        "for shell_file in \"${shell_files[@]}\"; do bash -n \"$shell_file\"; done",
        "PYTHONPYCACHEPREFIX=/tmp/codex/kei/pycache python3 -m py_compile",
        "shellcheck -x -P tests/shell:scripts:scripts/full-test \"${shell_files[@]}\"",
        "shfmt -d \"${shell_files[@]}\"",
        "ruff check \"${python_files[@]}\"",
        "actionlint .github/workflows/*.yml",
        "scripts/check-contracts",
        "bash scripts/check-roundtrip-gate.sh",
    ] {
        assert!(
            contributing.contains(expected),
            "CONTRIBUTING.md must document current gate command: {expected}"
        );
    }

    assert!(
        pr_template.contains("`just gate` passes"),
        "PR template should ask reviewers for the current local gate"
    );
    for expected in [
        "## Contract and risk",
        "## Regression proof",
        "independent/adversarial review results",
    ] {
        assert!(
            pr_template.contains(expected),
            "PR template must capture verification evidence: {expected}"
        );
    }
    assert!(
        !pr_template.contains("cargo test --bin kei --test cli --test behavioral"),
        "PR template must not keep stale partial test command"
    );
}

#[test]
fn repo_pr_ready_skill_uses_current_validation_workflow() {
    let gitignore = repo_file(".gitignore");
    assert!(
        !gitignore.lines().any(|line| line.trim() == ".agents/"),
        "repository skills must remain available for version control"
    );

    let skill = repo_file(".agents/skills/kei-pr-ready/SKILL.md");
    for expected in [
        "name: kei-pr-ready",
        "without publishing or changing it",
        "just agent-status",
        "just review-scope BASE=<resolved-base>",
        "coverage ledger",
        "validation provenance",
        "STALE",
        "OTHER BRANCH",
        "docs/architecture.md",
        "tests/README.md",
        "just test scenario NAME",
        "just gate",
        "final verdict: ready or not ready",
    ] {
        assert!(
            skill.contains(expected),
            "kei-pr-ready skill missing validation contract: {expected}"
        );
    }

    let metadata = repo_file(".agents/skills/kei-pr-ready/agents/openai.yaml");
    assert!(
        metadata.contains("Use $kei-pr-ready"),
        "skill metadata must keep its default invocation aligned with SKILL.md"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn review_scope_reports_exact_diff_workspace_and_validation_provenance() {
    let temp = tempfile::tempdir().expect("review scope tempdir");
    let repo = temp.path().join("repo");
    let runs = temp.path().join("runs");
    std::fs::create_dir_all(&repo).expect("create fixture repo");
    std::fs::create_dir_all(&runs).expect("create fixture runs directory");

    run_git(&repo, &["init", "-b", "main"]);
    run_git(&repo, &["config", "user.name", "Kei Test"]);
    run_git(&repo, &["config", "user.email", "kei-test@example.invalid"]);
    std::fs::write(repo.join("tracked.txt"), "base\n").expect("write base file");
    run_git(&repo, &["add", "tracked.txt"]);
    run_git(&repo, &["commit", "-m", "base"]);
    let base = run_git(&repo, &["rev-parse", "HEAD"]);

    run_git(&repo, &["checkout", "-b", "feature"]);
    std::fs::write(repo.join("tracked.txt"), "feature\n").expect("write feature file");
    std::fs::write(repo.join("added.txt"), "added\n").expect("write added file");
    run_git(&repo, &["add", "tracked.txt", "added.txt"]);
    run_git(&repo, &["commit", "-m", "feature"]);
    let head = run_git(&repo, &["rev-parse", "HEAD"]);
    std::fs::write(repo.join("tracked.txt"), "workspace\n").expect("write workspace file");
    std::fs::write(repo.join("untracked.txt"), "untracked\n").expect("write untracked file");

    let record = runs.join("latest.json");
    let abbreviated_head = head
        .get(..7)
        .expect("git SHA has at least seven ASCII bytes");
    write_json(
        &record,
        &serde_json::json!({
            "branch": "feature",
            "head": abbreviated_head,
            "end_head": head,
            "start_worktree_clean": true,
            "end_worktree_clean": true
        }),
    );
    let run_scope = || {
        Command::new(repo_path("scripts/just/review-scope.sh"))
            .arg("BASE=main")
            .current_dir(&repo)
            .env("KEI_FULL_TEST_RUNS_DIR", &runs)
            .output()
            .expect("run review scope helper")
    };

    let current = run_scope();
    assert!(
        current.status.success(),
        "review scope failed: {}",
        String::from_utf8_lossy(&current.stderr)
    );
    let current = command_text(&current);
    for expected in [
        "base_ref: main",
        &format!("base: {base}"),
        &format!("merge_base: {base}"),
        &format!("head: {head}"),
        "commits: 1",
        "A\tadded.txt",
        "M\ttracked.txt",
        " M tracked.txt",
        "?? untracked.txt",
        "validation_start_worktree_clean: true",
        "validation_end_worktree_clean: true",
        "validation_status: CURRENT",
    ] {
        assert!(
            current.contains(expected),
            "review scope output missing {expected}:\n{current}"
        );
    }

    write_json(
        &record,
        &serde_json::json!({"branch": "feature", "head": head, "end_head": head}),
    );
    assert!(
        command_text(&run_scope()).contains("validation_status: STALE"),
        "legacy records without worktree evidence must not be current"
    );

    write_json(
        &record,
        &serde_json::json!({
            "branch": "feature",
            "head": base,
            "end_head": base,
            "start_worktree_clean": true,
            "end_worktree_clean": true
        }),
    );
    assert!(
        command_text(&run_scope()).contains("validation_status: STALE"),
        "same-branch result at another head must be stale"
    );

    write_json(
        &record,
        &serde_json::json!({
            "branch": "other",
            "head": base,
            "end_head": base,
            "start_worktree_clean": true,
            "end_worktree_clean": true
        }),
    );
    assert!(
        command_text(&run_scope()).contains("validation_status: OTHER BRANCH"),
        "result from another branch must not prove the current head"
    );
}

#[test]
fn roundtrip_gate_documents_heuristic_limits_and_bypass_rationale() {
    let gate = repo_file("scripts/check-roundtrip-gate.sh");

    for expected in [
        "Heuristic diff guard for serializer changes",
        "intentionally heuristic",
        "false-positive",
        "false-negative",
        "review prompt, not proof that the code is wrong",
        "written reviewer rationale",
        "heuristic serializer change detected without a round-trip test edit",
    ] {
        assert!(
            gate.contains(expected),
            "roundtrip gate must document heuristic behavior and bypass rationale: {expected}"
        );
    }
}

#[test]
fn bug_report_template_requires_web_access_and_redaction() {
    let bug = repo_file(".github/ISSUE_TEMPLATE/bug_report.yml");

    for expected in [
        "I have confirmed that ADP is disabled",
        "required: true",
        "Redact Apple IDs, passwords, session cookies, bearer tokens, webhook URLs",
    ] {
        assert!(
            bug.contains(expected),
            "bug report template must keep triage and redaction guidance: {expected}"
        );
    }
}

#[test]
fn loopback_bound_tests_keep_explicit_skip_gate() {
    let helper = repo_file("src/test_helpers.rs");
    let metrics = repo_file("src/metrics.rs");
    let readme = repo_file("tests/README.md");

    for expected in [
        "loopback bind is not permitted on this host",
        "pub(crate) fn skip_if_loopback_bind_blocked",
        "pub(crate) async fn start_wiremock_or_skip",
        "macro_rules! start_wiremock_or_skip",
        "None => return",
    ] {
        assert!(
            helper.contains(expected),
            "loopback test helper must keep explicit skip support: {expected}"
        );
    }

    for expected in [
        "spawn_server_with_staleness_threshold_does_not_panic_inside_runtime",
        "spawn_server_serves_metrics_and_healthz_over_http",
        "skip_if_loopback_bind_blocked",
    ] {
        assert!(
            metrics.contains(expected),
            "metrics HTTP tests must remain covered by the loopback skip gate: {expected}"
        );
    }

    for expected in [
        "Some offline unit tests bind `127.0.0.1`",
        "Normal CI hosts still run the",
        "tests strictly; restricted sandboxes",
        "explicit skip line instead of a",
        "false bind failure",
    ] {
        assert!(
            readme.contains(expected),
            "tests/README.md must document loopback skip semantics: {expected}"
        );
    }
}

#[test]
fn audit_ignores_carry_removal_triggers() {
    let audit = repo_file(".cargo/audit.toml");

    for expected in [
        "Remove this ignore once little_exif drops paste",
        "Remove this ignore once reqwest's QUIC stack no longer pulls rand",
        "Remove these ignores once plist and little_exif can both resolve",
    ] {
        assert!(
            audit.contains(expected),
            "audit ignore must document removal trigger: {expected}"
        );
    }
}

#[test]
fn funding_file_contains_only_configured_sponsor_platforms() {
    let funding = repo_file(".github/FUNDING.yml");

    assert_eq!(
        funding.trim(),
        "ko_fi: rhoopr",
        "FUNDING.yml should not keep unconfigured GitHub template placeholders"
    );
}

#[test]
fn typed_error_downcasts_stay_in_named_classifier_boundaries() {
    let allowed = [
        "classify_api_error",
        "classify_auth_flow_error",
        "classify_auth_retry_error",
        "classify_cli_parse_exit",
        "classify_conditional_publish_error",
        "classify_download_task_error",
        "classify_exit_error",
        "classify_incremental_error",
        "classify_provider_lookup_error",
        "classify_rate_limit_error",
        "classify_srp_post_error",
        "classify_sync_auth_error",
        "is_session_error",
        "map_library_init_error",
    ]
    .into_iter()
    .map(String::from)
    .collect::<BTreeSet<_>>();
    let mut files = Vec::new();
    rust_files_under(&repo_path("src"), &mut files);

    let mut observed = BTreeSet::new();
    let mut violations = Vec::new();
    for path in files {
        let source = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
            .replace("\r\n", "\n")
            .replace('\r', "\n");
        let prod = production_source(&source);
        let mut current_fn: Option<String> = None;

        for (index, line) in prod.lines().enumerate() {
            if let Some(name) = function_name(line) {
                current_fn = Some(name);
            }
            if line.contains("downcast_ref::<") || line.contains(".downcast::<") {
                let Some(name) = current_fn.as_deref() else {
                    violations.push(format!(
                        "{}:{} downcast outside a function: {}",
                        path.strip_prefix(env!("CARGO_MANIFEST_DIR"))
                            .unwrap_or(path.as_path())
                            .display(),
                        index + 1,
                        line.trim()
                    ));
                    continue;
                };
                if allowed.contains(name) {
                    observed.insert(name.to_string());
                } else {
                    violations.push(format!(
                        "{}:{} downcast in {name}: {}",
                        path.strip_prefix(env!("CARGO_MANIFEST_DIR"))
                            .unwrap_or(path.as_path())
                            .display(),
                        index + 1,
                        line.trim()
                    ));
                }
            }
        }
    }

    assert!(
        violations.is_empty(),
        "production typed-error downcasts must stay in named classifiers or documented owner boundaries:\n{}",
        violations.join("\n")
    );
    assert_eq!(
        observed, allowed,
        "classifier inventory changed; update the #587 boundary list deliberately"
    );
}

#[test]
fn live_test_recipe_forces_all_features_after_nodefault_phase() {
    let justfile = repo_file("justfile");
    let live_case = justfile
        .split("live)")
        .nth(1)
        .and_then(|tail| tail.split(";;").next())
        .expect("justfile must have a live test recipe case");

    for suite in ["sync", "state_auth", "import_existing_live"] {
        let expected = format!("cargo test --all-features --test {suite}");
        assert!(
            live_case.contains(&expected),
            "`just test live` must rebuild {suite}'s child binary with XMP after full-test's no-default phase"
        );
    }
}

#[test]
fn live_import_smoke_uses_toml_directory() {
    let smokes = repo_file("scripts/full-test/run_live_smokes.sh");

    assert!(
        smokes.contains("import-existing --dry-run --recent 5 --config \"$sync_config\""),
        "import-existing live smoke must pass the generated TOML config"
    );
    assert!(
        !smokes.contains("import-existing --dry-run --recent 5 --download-dir"),
        "import-existing live smoke must not use the removed --download-dir flag"
    );
    assert!(
        smokes.contains(r#"${TMPDIR:-/tmp/codex/kei/full-test/tmp}/photos-test"#),
        "live smoke download scratch should follow full-test's TMPDIR"
    );
}

#[test]
fn live_import_rehearsal_seeds_album_with_per_filter_recent_scope() {
    let rehearsal = repo_file("scripts/full-test/run_live_import_rehearsal.sh");

    assert!(
        rehearsal.contains("sync --recent 10 --recent-scope per-filter --no-progress-bar"),
        "live import rehearsal must seed from the selected album's recent window, not the global library frontier"
    );
    assert!(
        rehearsal.contains("set +e\n    \"$@\" >\"$out\" 2>\"$err\"\n    local rc=$?\n    set -e"),
        "live import rehearsal must print command tails before propagating a failed command"
    );
    assert!(
        rehearsal.contains("import-existing --dry-run --recent 10 --force-empty --no-progress-bar"),
        "live import rehearsal dry-run should keep import-existing bounded to the same recent count"
    );
    assert!(
        rehearsal.contains("import-existing --recent 10 --force-empty --no-progress-bar"),
        "live import rehearsal real import should keep import-existing bounded to the same recent count"
    );
}

#[test]
fn full_test_cross_zone_album_phase_is_opt_in_and_checks_source_zone() {
    let run_all = repo_file("scripts/full-test/run_all.sh");
    let script = repo_file("scripts/full-test/run_cross_zone_album_hydration.sh");
    let readme = repo_file("tests/README.md");

    assert!(
        run_all.contains(r#"if [[ -n "${KEI_FULL_TEST_CROSS_ZONE_ALBUM:-}" ]]; then"#),
        "cross-zone live full-test phase must stay opt-in"
    );
    assert!(
        run_all.contains(
            r#"run_live_phase live_cross_zone_album -- "$script_dir/run_cross_zone_album_hydration.sh""#
        ),
        "cross-zone album phase must use live phase wrapping for prereq and rate-limit handling"
    );
    assert!(
        script.contains("libraries = [\"all\"]"),
        "cross-zone fixture sync must include all visible libraries"
    );
    assert!(
        script.contains("a.library <> 'PrimarySync'"),
        "cross-zone fixture assertion must prove a non-primary source zone"
    );
    assert!(
        script.contains("JOIN asset_albums aa"),
        "cross-zone fixture assertion must be tied to the selected album membership"
    );
    assert!(
        readme.contains("KEI_FULL_TEST_CROSS_ZONE_ALBUM"),
        "tests README must document the opt-in cross-zone fixture"
    );
}
