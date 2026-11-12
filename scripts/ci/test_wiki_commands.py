"""Exercise the live Home entry point with real Git and a source-bound fake builder."""

import copy
import json
import os
import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
SCRIPT = ROOT / "scripts/check-wiki-commands.sh"
COMMANDS = "sync login list password reset migrate-state config status doctor support-export manifest import-existing verify reconcile install uninstall service".split()


class WikiCommandsTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="kei-wiki-test-")
        self.addCleanup(self.temp.cleanup)
        self.work = Path(self.temp.name)
        self.repo, self.wiki, self.bin = (self.work / name for name in ["repo", "wiki", "bin"])
        self.real_git = shutil.which("git")
        self.log = self.work / "builds.jsonl"
        self.contract_path = self.work / "contract.json"
        self.bin.mkdir()
        self.repo.mkdir()
        self.wiki.mkdir()
        self.env = {**os.environ, "PATH": f"{self.bin}:{os.environ['PATH']}",
                    "RUNNER_TEMP": str(self.work), "BUILD_LOG": str(self.log), "REAL_GIT": self.real_git,
                    "ICLOUD_PASSWORD": "must-not-reach-help"}
        for path in [self.repo, self.wiki]:
            self.git(path, "init", "-q")
        self.stable = self.source("0.25.3", COMMANDS)
        self.beta = self.source("0.30.0-beta.1", COMMANDS[:-3] + ["service"])
        self.wrong = self.source("0.25.3", COMMANDS, marker="different source, same version")
        self.profiles = [
            {"heading": "Stable v0.25.3 commands", "version": "0.25.3", "source_revision": self.stable},
            {"heading": "Commands", "version": "0.30.0-beta.1", "source_revision": self.beta},
        ]
        self.home = "# Home\n\n```sh\nkei --version\nkei --help\n```\n"
        for profile, commands in zip(self.profiles, [COMMANDS, COMMANDS[:-3] + ["service"]], strict=True):
            self.home += f"\n## {profile['heading']}\n\nThis table describes v{profile['version']}.\n\n"
            self.home += "| Command | Description |\n|---------|-------------|\n"
            self.home += "".join(f"| [`{command}`](Command) | Description |\n" for command in commands)
        self.write_home(self.home)
        self.executable(self.bin / "cargo", '''#!/usr/bin/python3
import json, os, pathlib, subprocess, sys
source = pathlib.Path.cwd()
assert sys.argv[1:5] == ["build", "--locked", "--bin", "kei"]
with open(os.environ["BUILD_LOG"], "a") as log:
    log.write(json.dumps({"source": str(source), "sha": subprocess.check_output([os.environ["REAL_GIT"], "rev-parse", "HEAD"], text=True).strip()}) + "\\n")
data = json.loads((source / "fixture.json").read_text())
data["version"] = os.environ.get("BINARY_VERSION", data["version"])
if os.environ.get("EMPTY_HELP"):
    data["commands"] = []
target = pathlib.Path(sys.argv[sys.argv.index("--target-dir") + 1]) / "debug/kei"
target.parent.mkdir(parents=True, exist_ok=True)
target.write_text("#!/usr/bin/python3\\ndata = " + repr(data) + "\\n" + r"""
import os, sys
assert "ICLOUD_PASSWORD" not in os.environ
assert os.environ["HOME"] == os.environ["XDG_CONFIG_HOME"] == os.environ["KEI_DATA_DIR"]
args = sys.argv[1:]
if args == ["--version"]:
    print("kei " + data["version"])
elif args == ["--help"]:
    print("Commands:\\n" + "".join("  " + c + "  Description\\n" for c in data["commands"] + ["help"]) + "Options:\\n")
elif args in [["--version", "--help"], ["--help", "--help"]]:
    pass
elif len(args) == 2 and args[0] in data["commands"] and args[1] == "--help":
    pass
else:
    sys.exit(2)
""")
target.chmod(0o755)
''')

    def git(self, path, *args):
        return subprocess.check_output([self.real_git, "-C", str(path), *args], text=True,
                                       stderr=subprocess.DEVNULL).strip()

    def commit(self, path, *files):
        self.git(path, "add", *files)
        self.git(path, "-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid",
                 "commit", "-qm", "fixture")
        return self.git(path, "rev-parse", "HEAD")

    def source(self, version, commands, **extra):
        (self.repo / "Cargo.toml").write_text(f'[package]\nversion = "{version}"\n')
        (self.repo / "fixture.json").write_text(json.dumps({"version": version, "commands": commands, **extra}))
        return self.commit(self.repo, "Cargo.toml", "fixture.json")

    def write_home(self, home):
        (self.wiki / "Home.md").write_text(home)
        self.commit(self.wiki, "Home.md")

    @staticmethod
    def executable(path, text):
        path.write_text(text)
        path.chmod(0o755)

    def check(self):
        self.contract_path.write_text(json.dumps({"live_home_profiles": self.profiles}))
        return subprocess.run(["bash", str(SCRIPT), str(self.repo), str(self.wiki), str(self.contract_path)],
                              env=self.env, text=True, capture_output=True, timeout=30, check=False)

    def rejected(self, expected, before_build=False):
        result = self.check()
        self.assertNotEqual(result.returncode, 0, result.stdout)
        self.assertIn(expected, result.stderr)
        if before_build:
            self.assertFalse(self.log.exists(), self.log.read_text() if self.log.exists() else "")
        return result

    def test_both_profiles_have_source_and_wiki_bound_receipts(self):
        result = self.check()
        self.assertEqual(result.returncode, 0, result.stderr)
        receipts = [json.loads(line) for line in result.stdout.splitlines() if line.startswith("{")]
        self.assertEqual([r["commands"] for r in receipts], [17, 15])
        self.assertEqual([r["source_sha"] for r in receipts], [self.stable, self.beta])
        self.assertEqual([r["version"] for r in receipts], ["0.25.3", "0.30.0-beta.1"])
        for receipt in receipts:
            self.assertEqual(receipt["wiki_sha"], self.git(self.wiki, "rev-parse", "HEAD"))
            self.assertEqual(receipt["examples"], 2)
            self.assertRegex(receipt["binary_sha256"], r"^[0-9a-f]{64}$")
        builds = [json.loads(line) for line in self.log.read_text().splitlines()]
        self.assertEqual([build["sha"] for build in builds], [self.stable, self.beta])
        self.assertTrue(all(not Path(build["source"]).exists() for build in builds))
        self.assertEqual(self.git(self.repo, "rev-parse", "HEAD"), self.wrong)

    def test_shallow_candidate_fetches_exact_pins_from_its_origin(self):
        shallow = self.work / "shallow"
        subprocess.run([self.real_git, "clone", "--quiet", "--depth=1", self.repo.as_uri(), str(shallow)], check=True)
        self.repo = shallow
        missing = subprocess.run([self.real_git, "-C", str(shallow), "cat-file", "-e", self.beta],
                                 capture_output=True, check=False)
        self.assertNotEqual(missing.returncode, 0)
        result = self.check()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual([json.loads(line)["sha"] for line in self.log.read_text().splitlines()], [self.stable, self.beta])

    def test_wrong_command_is_rejected_in_each_table(self):
        for occurrence in [0, 1]:
            with self.subTest(occurrence=occurrence):
                parts = self.home.split("[`sync`]")
                self.write_home("[`sync`]".join(parts[:occurrence + 1]) + "[`syncc`]" + "[`sync`]".join(parts[occurrence + 1:]))
                self.rejected("Home command table differs")

    def test_missing_stable_and_beta_commands_are_rejected(self):
        for command in ["install", "doctor"]:
            with self.subTest(command=command):
                old = f"| [`{command}`](Command) | Description |\n"
                home = self.home.replace(old, "", 1) if command == "install" else self.home.rsplit(old, 1)[0] + self.home.rsplit(old, 1)[1]
                self.write_home(home)
                self.rejected("Home command table differs")

    def test_extra_beta_command_is_rejected(self):
        self.write_home(self.home + "| [`install`](Command) | Wrong beta command |\n")
        self.rejected("Home command table differs")

    def test_extra_unlinked_beta_command_is_rejected_before_build(self):
        self.write_home(self.home + "| `install` | Register service |\n")
        self.rejected("unsupported command table row", before_build=True)

    def test_other_table_rows_and_versions_do_not_enter_command_inventory(self):
        self.write_home(self.home + "\n| Flag | Description |\n|------|-------------|\n| `--log-level` | v9.0.0 |\n")
        result = self.check()
        self.assertEqual(result.returncode, 0, result.stderr)
        receipts = [json.loads(line) for line in result.stdout.splitlines() if line.startswith("{")]
        self.assertEqual([r["commands"] for r in receipts], [17, 15])

    def test_order_is_part_of_the_inventory(self):
        # Swap complete rows rather than sorting or accepting set equality.
        self.write_home(self.home.replace("| [`sync`](Command) | Description |\n| [`login`](Command) | Description |", "| [`login`](Command) | Description |\n| [`sync`](Command) | Description |", 1))
        self.rejected("Home command table differs")

    def test_missing_duplicate_and_empty_tables_fail_before_build(self):
        for home in [self.home.replace("## Commands", "## Missing"),
                     self.home + "\n## Commands\n", self.home.rsplit("| [`", 15)[0],
                     self.home.replace("| Command |", "| Wrong |", 1)]:
            with self.subTest(home=home):
                self.write_home(home)
                self.rejected("wiki command check:", before_build=True)

    def test_wrong_or_contradictory_version_claim_fails_before_build(self):
        for replacement in ["This table describes v9.0.0.", "This table describes v0.25.3 and v9.0.0.",
                            "This table describes v0.25.3+wrong.", "This table describes v0.25.3.1."]:
            with self.subTest(replacement=replacement):
                self.write_home(self.home.replace("This table describes v0.25.3.", replacement))
                self.rejected("Home version claim", before_build=True)

    def test_code_formatted_and_plain_contradictory_versions_fail_before_build(self):
        for claim in ["Use stable `v9.0.0`.", "Use stable 9.0.0."]:
            with self.subTest(claim=claim):
                self.write_home(self.home.replace("This table describes v0.25.3.", f"This table describes v0.25.3. {claim}"))
                self.rejected("Home version claim", before_build=True)

    def test_code_formatted_and_plain_matching_versions_pass(self):
        for claim in ["Use `v0.30.0-beta.1`.", "Use 0.30.0-beta.1."]:
            with self.subTest(claim=claim):
                self.write_home(self.home.replace("This table describes v0.30.0-beta.1.", claim))
                result = self.check()
                self.assertEqual(result.returncode, 0, result.stderr)

    def test_second_profile_source_version_is_checked_before_any_build(self):
        self.profiles[1]["source_revision"] = self.stable
        self.rejected("profile version differs from pinned source", before_build=True)

    def test_wrong_checkout_sha_with_same_version_fails_before_build(self):
        self.executable(self.bin / "git", '''#!/usr/bin/python3
import os, subprocess, sys
if "checkout" in sys.argv and os.environ["EXPECTED_SHA"] in sys.argv:
    source = sys.argv[sys.argv.index("-C") + 1]
    subprocess.run([os.environ["REAL_GIT"], "-C", source, "fetch", "--quiet", "--depth=1", os.environ["FIXTURE_ORIGIN"], os.environ["WRONG_SHA"]], check=True)
args = [os.environ["WRONG_SHA"] if arg == os.environ["EXPECTED_SHA"] and "checkout" in sys.argv else arg for arg in sys.argv[1:]]
os.execv(os.environ["REAL_GIT"], [os.environ["REAL_GIT"], *args])
''')
        self.env.update(EXPECTED_SHA=self.stable, WRONG_SHA=self.wrong, FIXTURE_ORIGIN=str(self.repo))
        self.rejected("checkout SHA differs", before_build=True)

    def test_branch_version_cannot_substitute_for_published_binary(self):
        self.env["BINARY_VERSION"] = "0.26.1-dev"
        self.rejected("binary version differs")

    def test_empty_binary_inventory_fails(self):
        self.env["EMPTY_HELP"] = "1"
        self.rejected("empty command inventory")

    def test_examples_never_fall_back_to_the_binary_that_accepts_them(self):
        self.write_home(self.home + "\n```sh\nkei install\n```\n")
        self.rejected("invalid Home.md example for 0.30.0-beta.1")

    def test_shared_command_example_is_parsed_by_both_sources(self):
        self.write_home(self.home + "\n```sh\nkei sync\n```\n")
        result = self.check()
        self.assertEqual(result.returncode, 0, result.stderr)
        receipts = [json.loads(line) for line in result.stdout.splitlines() if line.startswith("{")]
        self.assertEqual([r["examples"] for r in receipts], [3, 3])

    def test_stale_flag_and_shell_substitution_are_rejected_without_execution(self):
        marker = self.work / "must-not-exist"
        for example in ["kei sync --bogus", f"kei sync $(touch {marker})"]:
            with self.subTest(example=example):
                self.write_home(self.home + f"\n```sh\n{example}\n```\n")
                self.rejected("invalid Home.md example")
                self.assertFalse(marker.exists())

    def test_no_examples_and_dirty_wiki_are_rejected_before_build(self):
        self.write_home(self.home.replace("kei --version\nkei --help\n", ""))
        self.rejected("no command examples", before_build=True)
        (self.wiki / "Home.md").write_text(self.home)
        self.rejected("wiki checkout is not clean", before_build=True)

    def test_missing_duplicate_and_non_sha_profiles_fail_before_build(self):
        original = copy.deepcopy(self.profiles)
        for profiles in [[], [original[0], original[0]], [{**original[0], "source_revision": "main"}, original[1]], [original[0]]]:
            with self.subTest(profiles=profiles):
                self.profiles = profiles
                self.rejected("wiki command check:", before_build=True)

    def test_repository_contract_keeps_historical_and_live_pins_separate(self):
        contract = json.loads((ROOT / "docs/wiki-contract.json").read_text())
        self.assertEqual(contract["wiki_revision"], "c51f350bba04688b0d5f9abad1f469ea0bac5a59")
        self.assertEqual([(p["heading"], p["version"], p["source_revision"]) for p in contract["live_home_profiles"]], [
            ("Stable v0.25.3 commands", "0.25.3", "a2081bfd109993c76f34ac13b12ee15b48c86008"),
            ("Commands", "0.30.0-beta.1", "43c900b8b3e065aa86d79856b4f97965b60ee2d9"),
        ])

    def test_docs_workflow_retains_candidate_rustdoc_and_one_live_clone(self):
        workflow = (ROOT / ".github/workflows/checks.yml").read_text().split("  docs:\n", 1)[1].split("\n  catalogs:", 1)[0]
        self.assertEqual(workflow.count("git clone"), 1)
        self.assertIn('scripts/check-wiki-commands.sh "$GITHUB_WORKSPACE" "$RUNNER_TEMP/kei-wiki"', workflow)
        self.assertIn("cargo doc --no-deps --all-features", workflow)
        self.assertNotIn("cargo build", workflow)


if __name__ == "__main__":
    unittest.main()
