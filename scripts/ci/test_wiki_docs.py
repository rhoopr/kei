"""Regression fixtures for revision-scoped documentation checks; no provider calls."""

import copy
import json
import subprocess
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

import wiki_docs as docs


class WikiDocsTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.repo = Path(self.temp.name) / "repo"
        self.wiki = Path(self.temp.name) / "wiki"
        self.repo.mkdir()
        self.wiki.mkdir()
        owners = ["Cli", "PasswordArgs", "SyncArgs", "FriendlyArgs", "StatusArgs", "DoctorArgs",
                  "SupportExportArgs", "ManifestArgs", "ImportArgs", "VerifyArgs", "ReconcileArgs",
                  "MigrateStateArgs", "InstallArgs", "UninstallArgs"]
        cli = "\n".join(f"pub struct {name} {{\n    #[arg(long)]\n    pub dry_run: bool,\n}}" for name in owners)
        cli += "\npub enum Command {\n    Sync {\n}\n"
        (self.repo / "src").mkdir()
        (self.repo / "src/cli.rs").write_text(cli)
        (self.repo / "Cargo.toml").write_text('[package]\nversion = "1.0.0"\n')
        (self.repo / "example.config.toml").write_text(
            '# Option: watch.interval\n[watch]\ninterval = 0\n')
        self.revision = self.commit(self.repo)
        (self.wiki / "Home.md").write_text('# Home\n\n## Commands\n\n| [`sync`](Sync) | Sync |\n')
        (self.wiki / "Sync.md").write_text('# Sync\n\n## Usage\n\n```sh\nkei sync --dry-run\n```\n')
        (self.wiki / "Configuration.md").write_text(
            '# Configuration\n\n| `watch.interval` | 0 |\n\n```toml\n[watch]\ninterval = 10\n```\n')
        self.wiki_revision = self.commit(self.wiki)
        self.contract = {"schema_version": 1, "stable": {"version": "1.0.0", "source_revision": self.revision},
                         "wiki_revision": self.wiki_revision,
                         "compatibility_revision": self.wiki_revision,
                         "historical_examples": [], "actions": {}, "command_fixtures": []}

    @staticmethod
    def commit(path):
        def git(*args):
            return subprocess.check_output(["git", "-C", str(path), *args], text=True,
                                           stderr=subprocess.DEVNULL)
        git("init", "-q")
        git("add", "Home.md", "Sync.md", "Configuration.md") if (path / "Home.md").exists() else git(
            "add", "Cargo.toml", "example.config.toml", "src/cli.rs")
        git("-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid",
            "commit", "-qm", "fixture")
        return git("rev-parse", "HEAD").strip()

    def check(self):
        return docs.validate(self.repo, self.wiki, self.contract)

    def mutate(self, file, text):
        path = self.wiki / file
        path.write_text(path.read_text() + text)

    def test_complete_offline_contract(self):
        result = self.check()
        self.assertEqual((result["pages"], result["commands"], result["toml_examples"]), (3, 1, 1))

    def test_broken_page_and_anchor_are_rejected(self):
        for target in ["Absent", "Sync#absent", "#absent"]:
            with self.subTest(target=target), self.assertRaises(docs.ContractError):
                docs.check_links({"Home": f"# Home\n[go]({target})", "Sync": "# Sync"}, self.repo)

    def test_wiki_syntax_unicode_duplicate_and_code_headings(self):
        pages = {"Home": "# Home\n[[label|CLI‑flag#same-1]]", "CLI‑flag":
                 "# Same\n## Same\n```sh\n# hidden\n```"}
        docs.check_links(pages, self.repo)
        self.assertEqual(docs.anchors(pages["CLI‑flag"]), {"same", "same-1"})

    def test_malformed_and_reference_links(self):
        for text in ["[bad](Sync", "[bad](https:/broken)", "[bad][missing]", "[bad](https://github.com/rhoopr/kei/blob/main/README.md)"]:
            with self.subTest(text=text), self.assertRaises(docs.ContractError):
                docs.check_links({"Home": text}, self.repo)
        docs.check_links({"Home": "[good][ref]\n[ref]: Home"}, self.repo)

    def test_source_path_and_anchor_regressions(self):
        prefix = f"https://github.com/rhoopr/kei/blob/{self.revision}/"
        for target in [prefix + "missing.md", prefix + "src/cli.rs#absent", prefix + "src/cli.rs#L99999"]:
            with self.subTest(target=target), self.assertRaises(docs.ContractError):
                docs.check_links({"Home": f"[source]({target})"}, self.repo)

    def test_removed_compatibility_page_and_heading_fail(self):
        for replacement in ["# Changed", "# Sync"]:
            with self.subTest(replacement=replacement), self.assertRaises(docs.ContractError):
                (self.wiki / "Sync.md").write_text(replacement)
                self.check()
        (self.wiki / "Sync.md").unlink()
        with self.assertRaises(docs.ContractError):
            self.check()

    def test_non_ascii_compatibility_filename_is_not_skipped(self):
        old = self.wiki / "CLI‑existing.md"
        old.write_text("# Existing\n\n## Details\n")
        subprocess.run(["git", "-C", str(self.wiki), "add", old.name], check=True,
                       capture_output=True)
        subprocess.run(["git", "-C", str(self.wiki), "-c", "user.name=Fixture", "-c",
                        "user.email=fixture@example.invalid", "commit", "-qm", "unicode"],
                       check=True, capture_output=True)
        revision = docs.git(self.wiki, "rev-parse", "HEAD").strip()
        self.contract.update(wiki_revision=revision, compatibility_revision=revision)
        self.check()
        old.unlink()
        with self.assertRaisesRegex(docs.ContractError, "lost compatibility"):
            self.check()

    def test_invalid_commands_actions_and_flags(self):
        surface = {"sync": {"dry-run"}, "login": {"help"}}
        for argv in [["missing"], ["sync", "--bogus"], ["login", "--save-password"],
                     ["login", "missing"]]:
            with self.subTest(argv=argv), self.assertRaises(docs.ContractError):
                docs.check_argv(argv, surface, {"login": ["get-code", "submit-code"]})

    def test_missing_option_value_and_action_after_options(self):
        surface = {"login": {"config"}}
        for argv in [["--config"], ["login", "--config"],
                     ["login", "--config="], ["login", "--config", "x", "unknown"]]:
            with self.subTest(argv=argv), self.assertRaises(docs.ContractError):
                docs.check_argv(argv, surface, {"login": ["get-code"]}, {"config"})
        docs.check_argv(["login", "--config", "x", "get-code"], surface,
                        {"login": ["get-code"]}, {"config"})

    def test_invalid_example_is_attributed_to_page(self):
        self.mutate("Sync.md", '\n```sh\nkei sync --bogus\n```\n')
        with self.assertRaisesRegex(docs.ContractError, "Sync:.*unsupported"):
            self.check()

    def test_wrappers_are_words_not_shell_execution(self):
        self.assertEqual(docs.command_argv('docker exec --user 1000:1000 kei kei login get-code'),
                         ["login", "get-code"])
        self.assertEqual(docs.command_argv('docker compose run --rm kei sync --dry-run'),
                         ["sync", "--dry-run"])
        self.assertIsNone(docs.command_argv("docker compose up -d kei"))
        self.assertIsNone(docs.command_argv("docker pull ghcr.io/rhoopr/kei:0.25.3"))
        self.assertEqual(docs.command_argv("sudo kei install --system"), ["install", "--system"])
        self.assertEqual(docs.command_argv('kei login submit-code "123 456"'),
                         ["login", "submit-code", "123 456"])
        with self.assertRaises(docs.ContractError):
            docs.command_argv("kei sync 'unterminated")

    def test_invalid_toml_unknown_key_and_wrong_type(self):
        for text in ['[watch', '[watch]\nbogus = 1', '[watch]\ninterval = "ten"']:
            with self.subTest(text=text), self.assertRaises(docs.ContractError):
                docs.check_toml(text, {"watch.interval"}, {"watch.interval": 0})

    def test_missing_dictionary_key_is_rejected(self):
        (self.wiki / "Configuration.md").write_text("# Configuration\n")
        with self.assertRaisesRegex(docs.ContractError, "dictionary keys"):
            self.check()

    def test_extra_dictionary_key_is_rejected(self):
        self.mutate("Configuration.md", "\n| `watch.typo` | 0 |\n")
        with self.assertRaisesRegex(docs.ContractError, "extra=.*watch.typo"):
            self.check()

    def test_revision_version_and_historical_exclusion_drift(self):
        contract = copy.deepcopy(self.contract)
        self.contract["stable"]["version"] = "2.0.0"
        with self.assertRaisesRegex(docs.ContractError, "version"):
            self.check()
        self.contract = contract
        self.contract["historical_examples"] = [{"page": "Sync", "line": "kei old"}]
        with self.assertRaisesRegex(docs.ContractError, "stale historical"):
            self.check()

    def test_legacy_index_must_cover_every_retained_stub(self):
        (self.wiki / "CLI‑old.md").write_text("# Old\nThis flag was removed in v0.20.\n")
        with self.assertRaisesRegex(docs.ContractError, "legacy migration index"):
            self.check()
        (self.wiki / "Legacy-Flags.md").write_text("# Older flags\n[old](CLI‑old)\n")
        self.assertEqual(self.check()["legacy_pages_indexed"], 1)

    def test_wrong_wiki_revision_is_rejected(self):
        self.contract["wiki_revision"] = "f" * 40
        with self.assertRaisesRegex(docs.ContractError, "wiki HEAD"):
            self.check()

    def test_flattened_and_subcommand_fields_are_not_flags(self):
        cli = "pub struct Sample {\n    #[command(flatten)]\n    pub inner: Inner,\n    #[arg(long)]\n    pub real: bool,\n}"
        self.assertEqual(docs.struct_fields(cli, "Sample"), {"real"})

    def test_unclosed_fence_is_rejected(self):
        self.mutate("Sync.md", "\n```toml\n[watch]\ninterval = 2\n")
        with self.assertRaisesRegex(docs.ContractError, "unclosed"):
            self.check()

    def test_binary_mode_only_executes_help_with_clean_environment(self):
        with patch.object(docs.subprocess, "run") as run:
            run.return_value.returncode = 0
            self.assertTrue(docs.binary_help(Path("/fixture/kei"), ["sync", "--dry-run"]))
            args, kwargs = run.call_args
            self.assertEqual(args[0], ["/fixture/kei", "sync", "--dry-run", "--help"])
            self.assertNotIn("ICLOUD_PASSWORD", kwargs["env"])
            self.assertNotIn("shell", kwargs)
            self.assertEqual(kwargs["timeout"], 15)

    def test_contract_json_is_versioned_and_fixtures_declare_effects(self):
        path = Path(__file__).resolve().parents[2] / "docs/wiki-contract.json"
        contract = json.loads(path.read_text())
        self.assertEqual(contract["schema_version"], 1)
        self.assertRegex(contract["stable"]["source_revision"], r"^[0-9a-f]{40}$")
        for fixture in contract["command_fixtures"]:
            self.assertTrue(fixture["page"])
            self.assertTrue(fixture["without_help_effects"])
            self.assertTrue(fixture["argv"])


if __name__ == "__main__":
    unittest.main()
