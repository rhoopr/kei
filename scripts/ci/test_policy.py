"""Routing acceptance cases and the real CLI with a temporary git repository."""

import json
import os
import subprocess
import tempfile
import unittest
from pathlib import Path

from gate import check
from policy import ALWAYS, route


class PolicyTests(unittest.TestCase):
    def test_pr_categories(self):
        cases = [
            (["README.md", "docs/ci.md"], False, False, False),
            (["src/auth/session.rs"], True, False, False),
            (["src/download/file/publication.rs"], True, True, True),
            (["src/state/db.rs"], True, True, True),
            (["src/lib.rs"], True, True, False),
            (["src/cli.rs"], True, True, False),
            (["src/types.rs"], True, True, True),
            (["src/fs_util.rs"], True, True, True),
            (["src/service/windows.rs"], True, True, False),
            (["src/icloud/photos/asset.rs"], True, False, True),
            (["Cargo.lock"], True, True, True),
            ([".dockerignore"], True, True, True),
            ([".gitignore"], True, True, True),
            ([".github/workflows/ci.yml"], True, True, True),
            (["scripts/check-contracts"], False, False, False),
            (["unrecognized-input"], True, True, True),
            (["tests/cli.rs"], True, True, False),
        ]
        for files, code, platform, stress in cases:
            with self.subTest(files=files):
                result = route(files, "pull_request", False, True, False)
                self.assertEqual((result["code"], result["platform"], result["stress"]), (code, platform, stress))
                self.assertFalse(result["coverage"])
                self.assertEqual("copies-stress" in result["required"], stress)
                self.assertEqual("test_no_default" in result["required"], code)

    def test_notification_and_advisory_prs_keep_only_relevant_checks(self):
        # Exact GitHub file sets at 6e32a20 (#1000), 4f1dbc8 (#997),
        # f555e9a (#991) and fbda631 (#1001).
        cases = [
            (1000, ["docs/ci.md", "scripts/ci/nightly.py", "scripts/ci/test_nightly.py",
                    "src/inventory_diagnostic/tests.rs"], True),
            (997, ["docs/opencode-review.md", "scripts/ci/opencode_review.py",
                   "scripts/ci/test_opencode_review.py"], False),
            (1001, [".github/workflows/opencode-review.yml", "docs/architecture.md",
                    "docs/opencode-review.md", "scripts/ci/opencode_review.py",
                    "scripts/ci/test_opencode_review.py"], False),
            (991, [".github/workflows/opencode-review.yml", "docs/architecture.md",
                   "docs/ci.md", "docs/opencode-review.md", "scripts/ci/opencode_review.py",
                   "scripts/ci/test_opencode_review.py"], False),
        ]
        for pr, files, rust in cases:
            for light in (True, False):
                with self.subTest(pr=pr, light=light):
                    result = route(files, "pull_request", False, light, False)
                    self.assertEqual(result["code"], rust)
                    self.assertEqual(result["cargo"], rust)
                    self.assertEqual(result["platform"], rust)
                    self.assertFalse(result["qualification"])
                    self.assertFalse(result["stress"])
                    self.assertEqual(result["coverage"], rust and not light)
                    self.assertEqual(result["unused"], rust and not light)
                    for required in ALWAYS:
                        self.assertIn(required, result["required"])
                    for required in ("test", "test_no_default", "clippy", "catalogs", "lockfile", "security"):
                        self.assertEqual(required in result["required"], rust)
                    # Unrouted checks may skip; every selected job still fails closed.
                    needs = {name: {"result": "success"} for name in result["required"]}
                    needs["copies-stress"] = {"result": "skipped"}
                    check(needs, result["required"])
                    for required in result["required"]:
                        with self.subTest(required=required), self.assertRaises(ValueError):
                            check({**needs, required: {"result": "skipped"}}, result["required"])

    def test_go_cleanup_and_retired_probe_deletion_stay_maintenance(self):
        files = [".github/workflows/opencode-cli-smoke.yml",
                 ".github/workflows/opencode-review.yml", "docs/architecture.md",
                 "docs/opencode-review.md", "scripts/ci/opencode_review.py",
                 "scripts/ci/policy.py", "scripts/ci/test_opencode_cli_smoke.py",
                 "scripts/ci/test_opencode_review.py", "scripts/ci/test_policy.py"]
        for light in (False, True):
            with self.subTest(light=light):
                result = route(files, "pull_request", False, light, False)
                self.assertEqual(result["required"], ALWAYS)
                for key in ("code", "cargo", "platform", "stress", "qualification", "coverage", "unused"):
                    self.assertFalse(result[key], key)
        # Unknown workflow changes still require full qualification.
        self.assertTrue(route([".github/workflows/unknown.yml"], "pull_request", False, True, False)
                        ["qualification"])

    def test_known_ci_maintenance_and_rust_check_tools(self):
        maintenance = ["scripts/ci/nightly.py", "scripts/ci/opencode_review.py",
                       "scripts/ci/policy.py", "scripts/ci/gate.py", "scripts/ci/local_release.py",
                       "scripts/ci/test_qualification.py", "scripts/ci/test_new_notification.py",
                       "scripts/check-contracts",
                       ".github/scripts/check_workflow_hardening.py",
                       ".github/workflows/opencode-review.yml", ".github/workflows/nightly-health.yml",
                       ".github/workflows/coverage-comment.yml"]
        for path in maintenance:
            with self.subTest(path=path):
                result = route([path], "pull_request", False, True, False)
                self.assertEqual(result["required"], ALWAYS)
                for key in ("code", "platform", "stress", "qualification", "coverage", "unused"):
                    self.assertFalse(result[key], key)
        rust_tools = ["scripts/check-roundtrip-gate.sh", "scripts/test-scenarios/check.sh",
                      "scripts/test-copies-stress.sh"]
        for path in rust_tools:
            with self.subTest(path=path):
                result = route([path], "pull_request", False, True, False)
                self.assertTrue(result["code"])
                self.assertTrue(result["platform"])
                self.assertFalse(result["qualification"])
                self.assertEqual(result["stress"], path in {
                    "scripts/test-copies-stress.sh"})

    def test_build_fuzz_and_unknown_inputs_still_qualify(self):
        for path in (".github/workflows/ci.yml", ".github/workflows/checks.yml", "Cargo.toml", "Cargo.lock", "build.rs", "justfile", ".cargo/config.toml",
                     "Dockerfile", "docker/entrypoint.sh", ".gitignore", ".dockerignore",
                     "fuzz/fuzz_targets/auth_responses.rs", "fuzz/seeds/cloudkit_json/record.json",
                     "scripts/ci/artifact.py", "scripts/fixtures/check_source_package.sh",
                     ".github/scripts/patch_coverage.py",
                     ".github/workflows/qualification.yml", ".github/workflows/artifacts.yml",
                     ".github/workflows/release.yml", ".github/workflows/nightly.yml",
                     ".github/workflows/service-smoke.yml", ".github/workflows/new-build.yml",
                     "scripts/new-build-helper", "unrecognized-input"):
            with self.subTest(path=path):
                result = route([path], "pull_request", False, True, False)
                self.assertTrue(result["qualification"])
                self.assertTrue(result["code"])
                self.assertTrue(result["stress"])
        mixed = route(["scripts/ci/nightly.py", "src/download/pipeline/task.rs"],
                      "pull_request", False, True, False)
        self.assertTrue(mixed["stress"])
        self.assertFalse(mixed["qualification"])
        for files in (["scripts/ci/nightly.py"], ["src/inventory_diagnostic/tests.rs"], ["README.md"]):
            for heavy, full in ((True, False), (False, True)):
                result = route(files, "pull_request", heavy, True, full)
                for key in ("code", "cargo", "stress", "platform", "coverage", "unused", "qualification"):
                    self.assertTrue(result[key], (files, key))

    def test_native_service_routes_cover_dispatch_and_skip_unrelated_changes(self):
        selected = ["src/service/linux.rs", "src/service/new_platform.rs",
                    "src/commands/service.rs", "src/commands/service/types.rs",
                    "src/commands/status.rs", "src/commands/status/display.rs",
                    "src/cli.rs", "src/config.rs", "src/config/new_owner.rs", "src/lib.rs"]
        for path in selected:
            with self.subTest(path=path):
                result = route([path], "pull_request", False, True, False)
                self.assertTrue(result["service"])
                self.assertFalse(result["qualification"])
        for path in ["README.md", "docs/architecture.md", "scripts/check-contracts",
                     "src/auth/session.rs", "src/download/file/publication.rs",
                     "src/inventory_diagnostic/tests.rs", "scripts/ci/nightly.py"]:
            self.assertFalse(route([path], "pull_request", False, True, False)["service"], path)
        for path in ["Cargo.toml", ".github/workflows/service-smoke.yml", "unknown-input"]:
            result = route([path], "pull_request", False, True, False)
            self.assertTrue(result["service"])
            self.assertTrue(result["qualification"])
        # Main push keeps its existing broad quality route; full/native service
        # qualification remains independently required by nightly and release.
        self.assertFalse(route([], "push", False, True, False)["service"])
        self.assertTrue(route([], "push", True, True, False)["service"])

    def test_actual_pr_file_sets_keep_required_checks(self):
        # Exact GitHub file lists at heads 88ea35d (#984) and 30fe0e4 (#985).
        cases = [
            (984, [
                "docs/architecture.md",
                "src/download/orchestration/delta/album_progress_tests.rs",
                "src/download/orchestration/delta/tests.rs",
                "src/icloud/photos/album/changes.rs",
                "src/icloud/photos/album/hydration.rs",
                "src/icloud/photos/hydration_progress.rs",
                "src/icloud/photos/mod.rs",
                "src/icloud/photos/session.rs",
            ], True, True),
            (985, [
                "docs/architecture.md", "src/cli.rs", "src/commands/reset.rs",
                "src/lib.rs", "src/state/db/account/tests.rs",
                "tests/behavioral/state.rs", "tests/state_auth.rs",
            ], True, False),
        ]
        for pr, files, platform, stress in cases:
            with self.subTest(pr=pr):
                result = route(files, "pull_request", False, True, False)
                self.assertTrue(result["code"])
                self.assertTrue(result["cargo"])
                self.assertEqual(result["platform"], platform)
                self.assertEqual(result["stress"], stress)
                self.assertEqual("copies-stress" in result["required"], stress)
                self.assertFalse(result["qualification"])
                self.assertFalse(result["coverage"])
                self.assertFalse(result["unused"])
                for check in ("test", "test_no_default", "catalogs", "security", "lockfile"):
                    self.assertIn(check, result["required"])

    def test_stress_runtime_owners_and_fixture_helpers(self):
        paths = [
            "src/download/orchestration/generation.rs",
            "src/download/orchestration/queue_projection.rs",
            "src/download/orchestration/queue_projection/tests.rs",
            "src/download/orchestration/queue_projection/tests/late_io.rs",
            "src/download/orchestration/test_support.rs",
            "src/download/pipeline/consumer.rs", "src/download/file/publication.rs",
            "src/download/metadata_rewrite/queued.rs",
            "src/download/new_writer_owner.rs", "src/download/pipeline/tests/fixture.bin",
            "src/state/db/provider_selection.rs",
            "src/state/db/provider_inbox.rs", "src/state/db/provider_generations.rs",
            "src/state/db.rs", "src/state/schema.rs", "src/state/new_store.rs",
            "src/icloud/photos/asset.rs", "src/icloud/photos/album/lookup.rs",
            "src/icloud/photos/album/hydration.rs", "src/icloud/photos/session.rs",
            "src/icloud/photos/inbox.rs", "src/icloud/photos/new_adapter.rs",
            "src/icloud/error.rs",
            "src/commands/service/types.rs", "src/sync_loop/runner.rs",
            "src/sync_cycle.rs", "src/config.rs", "src/retry.rs",
            "src/selection.rs", "src/types.rs", "src/fs_util.rs",
            "src/test_helpers.rs", "tests/data/media/pattern.jpg",
        ]
        for path in paths:
            with self.subTest(path=path):
                result = route([path], "pull_request", False, True, False)
                self.assertTrue(result["stress"])
                self.assertIn("copies-stress", result["required"])

    def test_stress_skips_outer_commands_and_other_unit_test_modules(self):
        paths = [
            "src/cli.rs", "src/lib.rs", "src/commands/reset.rs",
            "src/commands/status.rs", "src/commands/list.rs", "src/account.rs",
            "src/icloud/service.rs", "src/auth/session.rs",
            "src/state/db/account/tests.rs", "src/state/db/connection_tests.rs",
            "src/download/orchestration/delta/album_progress_tests.rs",
            "src/download/orchestration/delta/tests.rs",
            "src/icloud/photos/album/lookup/tests.rs",
        ]
        for path in paths:
            with self.subTest(path=path):
                result = route([path], "pull_request", False, True, False)
                self.assertTrue(result["code"])
                self.assertFalse(result["stress"])
                self.assertNotIn("copies-stress", result["required"])
                self.assertIn("test", result["required"])
                self.assertIn("test_no_default", result["required"])
        mixed = route(["src/commands/reset.rs", "src/download/planner.rs"],
                      "pull_request", False, True, False)
        self.assertTrue(mixed["stress"])

    def test_stress_unknown_inputs_and_non_pr_events_fail_safe(self):
        for files in (["unknown-owner.rs"], ["new-build-input"], ["scripts/new-check"]):
            result = route(files, "pull_request", False, True, False)
            self.assertTrue(result["qualification"])
            self.assertTrue(result["stress"])
        for event in ("push", "schedule", "workflow_dispatch"):
            result = route([], event, False, True, False)
            self.assertTrue(result["stress"], event)

    def test_rollout_and_explicit_full_override(self):
        for light, full in [(False, False), (True, True)]:
            result = route(["src/auth/session.rs"], "pull_request", False, light, full)
            self.assertTrue(result["platform"])
            self.assertEqual(result["stress"], full)
            self.assertTrue(result["coverage"])

    def test_heavy_has_no_path_skips(self):
        for event in ("pull_request", "push", "schedule", "workflow_dispatch"):
            result = route([], event, True, True, False)
            for key in ("code", "cargo", "platform", "stress", "coverage", "unused"):
                self.assertTrue(result[key], (event, key))
            self.assertIn("catalogs", result["required"])
            self.assertIn("coverage-head", result["required"])

    def test_cli_event_merge_keeps_deleted_owner_and_excludes_base_only_input(self):
        script = Path(__file__).with_name("policy.py").resolve()
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            def git(*args):
                return subprocess.check_output(["git", *args], cwd=root, text=True).strip()
            git("init", "-q", "-b", "main")
            git("config", "user.name", "CI Test")
            git("config", "user.email", "ci@example.invalid")
            writer = root / "src/download/file/old.rs"
            writer.parent.mkdir(parents=True)
            writer.write_text("// synthetic writer owner\n")
            git("add", "src/download/file/old.rs")
            git("commit", "-qm", "base")
            git("checkout", "-qb", "topic")
            git("rm", "-q", "src/download/file/old.rs")
            (root / "README.md").write_text("synthetic topic docs\n")
            git("add", "README.md")
            git("commit", "-qm", "delete writer")
            git("checkout", "-q", "main")
            (root / "unknown-base-input").write_text("base-only input\n")
            git("add", "unknown-base-input")
            git("commit", "-qm", "advance base")
            base = git("rev-parse", "HEAD")
            git("merge", "--no-ff", "-qm", "synthetic event merge", "topic")
            output = root / "outputs"
            subprocess.run(["python3", str(script)], cwd=root, env={**os.environ,
                "EVENT_NAME": "pull_request", "BASE_SHA": base, "LIGHT_ENABLED": "true",
                "HEAVY": "false", "FULL": "false", "GITHUB_OUTPUT": str(output)}, check=True)
            values = dict(line.split("=", 1) for line in output.read_text().splitlines())
            self.assertEqual(values["stress"], "true")
            self.assertEqual(values["qualification"], "false")

    def test_cli_reads_real_git_diff(self):
        script = Path(__file__).with_name("policy.py").resolve()
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            def git(*args):
                return subprocess.check_output(["git", *args], cwd=root, text=True).strip()
            git("init", "-q")
            git("-c", "user.name=CI Test", "-c", "user.email=ci@example.invalid", "commit", "--allow-empty", "-qm", "base")
            base = git("rev-parse", "HEAD")
            (root / "src").mkdir()
            (root / "src" / "auth").mkdir()
            (root / "src" / "auth" / "session.rs").write_text("// fixture\n")
            git("add", "src/auth/session.rs")
            git("-c", "user.name=CI Test", "-c", "user.email=ci@example.invalid", "commit", "-qm", "change")
            output = root / "outputs"
            subprocess.run(["python3", str(script)], cwd=root, env={**os.environ,
                "EVENT_NAME": "pull_request", "BASE_SHA": base, "LIGHT_ENABLED": "true",
                "HEAVY": "false", "FULL": "false", "GITHUB_OUTPUT": str(output)}, check=True)
            values = dict(line.split("=", 1) for line in output.read_text().splitlines())
            self.assertEqual(values["platform"], "false")
            self.assertIn("test_no_default", json.loads(values["required"]))
            # A move out of a writer owner must retain the old path as a trigger.
            writer = root / "src" / "download" / "pipeline" / "task.rs"
            writer.parent.mkdir(parents=True)
            writer.write_text("// writer fixture\n" * 100)
            git("add", "src/download/pipeline/task.rs")
            git("-c", "user.name=CI Test", "-c", "user.email=ci@example.invalid", "commit", "-qm", "writer")
            writer_base = git("rev-parse", "HEAD")
            git("mv", "src/download/pipeline/task.rs", "src/auth/renamed.rs")
            git("-c", "user.name=CI Test", "-c", "user.email=ci@example.invalid", "commit", "-qm", "move writer")
            self.assertIn("rename", git("diff", "--summary", writer_base, "HEAD"))
            moved_output = root / "moved-outputs"
            subprocess.run(["python3", str(script)], cwd=root, env={**os.environ,
                "EVENT_NAME": "pull_request", "BASE_SHA": writer_base, "LIGHT_ENABLED": "true",
                "HEAVY": "false", "FULL": "false", "GITHUB_OUTPUT": str(moved_output)}, check=True)
            moved = dict(line.split("=", 1) for line in moved_output.read_text().splitlines())
            self.assertEqual(moved["stress"], "true")
            self.assertIn("copies-stress", json.loads(moved["required"]))
            # A known script-only change must reach the real lightweight CLI route.
            maintenance_base = git("rev-parse", "HEAD")
            script_dir = root / "scripts" / "ci"
            script_dir.mkdir(parents=True)
            (script_dir / "nightly.py").write_text("# notification fixture\n")
            git("add", "scripts/ci/nightly.py")
            git("-c", "user.name=CI Test", "-c", "user.email=ci@example.invalid", "commit", "-qm", "notification")
            maintenance_output = root / "maintenance-outputs"
            subprocess.run(["python3", str(script)], cwd=root, env={**os.environ,
                "EVENT_NAME": "pull_request", "BASE_SHA": maintenance_base, "LIGHT_ENABLED": "true",
                "HEAVY": "false", "FULL": "false", "GITHUB_OUTPUT": str(maintenance_output)}, check=True)
            maintained = dict(line.split("=", 1) for line in maintenance_output.read_text().splitlines())
            self.assertEqual(maintained["qualification"], "false")
            self.assertEqual(maintained["code"], "false")
            self.assertEqual(json.loads(maintained["required"]), ALWAYS)
            failed = subprocess.run(["python3", str(script)], cwd=root, env={**os.environ,
                "EVENT_NAME": "pull_request", "BASE_SHA": "invalid", "GITHUB_OUTPUT": str(root / "missing")}, capture_output=True, check=False)
            self.assertNotEqual(failed.returncode, 0)
            self.assertFalse((root / "missing").exists())


if __name__ == "__main__":
    unittest.main()
