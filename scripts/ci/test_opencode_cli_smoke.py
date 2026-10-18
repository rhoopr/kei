"""Offline execution of the exact inline smoke runner with a fake official CLI."""
import json
import os
import stat
import tempfile
import textwrap
import types
import unittest
from pathlib import Path
from unittest.mock import patch

WORKFLOW = Path(__file__).resolve().parents[2] / ".github/workflows/opencode-cli-smoke.yml"
TEXT = WORKFLOW.read_text()
SOURCE = textwrap.dedent(TEXT.split("          python3 -I - <<'PY'\n", 1)[1].rsplit("          PY\n", 1)[0])
runner = types.ModuleType("cli_smoke_fixture")
exec(compile(SOURCE, str(WORKFLOW), "exec"), runner.__dict__)  # noqa: S102 - trusted workflow fixture

MODEL = "glm-5.3-flash-free"
EVENTS = [
    {"type": "text", "part": {"text": "TEST_OK"}},
    {"type": "step_finish", "part": {"reason": "stop"}},
]


def events(values):
    return "\n".join(json.dumps(item) for item in values).encode()


class SmokeTests(unittest.TestCase):
    def test_only_manual_default_branch_no_checkout_secrets_or_write_token(self):
        self.assertIn("on:\n  workflow_dispatch:\n", TEXT)
        self.assertIn("permissions: {}", TEXT)
        self.assertIn("github.event.repository.default_branch", TEXT)
        for forbidden in ("secrets.", "actions/checkout", "github.token", "id-token:", ": write", "@latest"):
            self.assertNotIn(forbidden, TEXT)
        self.assertIn("fail-fast: false", TEXT)
        self.assertIn("sha256sum --check --status", TEXT)

    def test_exact_settings_keep_only_selected_free_model(self):
        for model in runner.MODELS:
            config = runner.settings(model)
            self.assertEqual(config["provider"]["opencode"]["whitelist"], [model])
            self.assertEqual(config["enabled_providers"], ["opencode"])
            self.assertEqual(config["small_model"], "opencode/" + model)
            self.assertEqual(config["permission"], {"*": "deny"})
            self.assertEqual(config["agent"]["smoke"]["permission"], {"*": "deny"})
            self.assertEqual(config["agent"]["smoke"]["steps"], 1)
            self.assertFalse(config["experimental"]["openTelemetry"])
            self.assertEqual(config["share"], "disabled")

    def test_real_call_path_with_isolated_environment_and_no_credentials(self):
        with tempfile.TemporaryDirectory() as name:
            fake = Path(name) / "opencode"
            fake.write_text("#!/usr/bin/python3\n" + textwrap.dedent("""
                import json, os, sys
                from pathlib import Path
                assert not list(Path.cwd().iterdir())
                assert sys.argv[1:5] == ["run", "--format", "json", "--model"]
                assert sys.argv[-1] == "test"
                assert not any(key in os.environ for key in ("GH_TOKEN", "GITHUB_TOKEN", "OPENCODE_API_KEY"))
                assert os.environ["OPENCODE_PURE"] == "1"
                assert os.environ["OPENCODE_DISABLE_DEFAULT_PLUGINS"] == "1"
                assert json.loads(os.environ["OPENCODE_CONFIG_CONTENT"])["permission"] == {"*": "deny"}
                print(json.dumps({"type":"text","part":{"text":"TEST_OK"}}))
                print(json.dumps({"type":"step_finish","part":{"reason":"stop"}}))
            """))
            fake.chmod(fake.stat().st_mode | stat.S_IXUSR)
            with patch.dict(os.environ, {"GH_TOKEN": "fixture-secret", "OPENCODE_API_KEY": "fixture-secret"}):
                result = runner.smoke(str(fake), MODEL)
            self.assertEqual(result["outcome"], "usable_completion")
            self.assertNotIn("fixture-secret", json.dumps(result))

    def test_nonterminal_tools_invalid_json_and_errors_fail_closed(self):
        for values in (EVENTS[:1], EVENTS + [{"type": "tool_use"}],
                       EVENTS + [{"type": "error", "error": {"data": {
                           "statusCode": 403, "responseBody": "FreeTierError fixture-secret"}}}]):
            result = runner.summarize(events(values), MODEL, 0)
            self.assertNotEqual(result["outcome"], "usable_completion")
            self.assertNotIn("fixture-secret", json.dumps(result))
        self.assertEqual(runner.summarize(b"not JSON", MODEL, 0)["outcome"], "invalid_cli_events")
        self.assertNotEqual(runner.summarize(events(EVENTS), MODEL, 1)["outcome"], "usable_completion")
        with self.assertRaises(ValueError):
            runner.smoke("unused", "glm-5.3-flash")

    def test_timeout_and_output_limit_kill_the_fake_cli(self):
        with tempfile.TemporaryDirectory() as name:
            fake = Path(name) / "opencode"
            fake.write_text("#!/usr/bin/python3\nimport time\ntime.sleep(30)\n")
            fake.chmod(fake.stat().st_mode | stat.S_IXUSR)
            with patch.object(runner, "TIMEOUT", 0.02):
                self.assertEqual(runner.smoke(str(fake), MODEL)["outcome"], "timeout")
            fake.write_text("#!/usr/bin/python3\nprint('x' * 1000)\n")
            with patch.object(runner, "LIMIT", 64):
                self.assertEqual(runner.smoke(str(fake), MODEL)["outcome"], "output_limit")


if __name__ == "__main__":
    unittest.main()
