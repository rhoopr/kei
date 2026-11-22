"""Each workflow must own a distinct trigger and route to the right caller."""

import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
WORKFLOWS = ROOT / ".github/workflows"


def workflow(name: str) -> str:
    return (WORKFLOWS / name).read_text()


def triggers(name: str) -> str:
    return workflow(name).split("\njobs:\n", maxsplit=1)[0]


class WorkflowOwnershipTests(unittest.TestCase):
    def test_heavy_qualification_is_called_or_manually_dispatched(self):
        text = triggers("qualification.yml")
        self.assertIn("workflow_call:", text)
        self.assertIn("workflow_dispatch:", text)
        self.assertNotRegex(text, r"(?m)^\s*schedule\s*:")

    def test_main_ci_routes_manual_dispatch_to_shared_checks(self):
        text = workflow("ci.yml")
        admission = triggers("ci.yml")
        self.assertRegex(admission, r"(?m)^\s*push:\s*$")
        self.assertRegex(admission, r"(?m)^\s*branches:\s*\[main\]\s*$")
        self.assertIn("workflow_dispatch:", admission)
        self.assertIn("uses: ./.github/workflows/checks.yml", text)
        self.assertNotRegex(text, r"uses:\s*\./\.github/workflows/qualification\.yml")

    def test_nightly_keeps_its_schedule_and_manual_dispatch(self):
        text = triggers("nightly.yml")
        self.assertRegex(text, r"(?m)^\s*schedule\s*:")
        self.assertRegex(text, r"(?m)^\s*-\s*cron:\s*\S")
        self.assertIn("workflow_dispatch:", text)

    def test_release_tags_still_call_heavy_qualification(self):
        self.assertRegex(
            workflow("release.yml"),
            r"(?m)^\s*uses:\s*\./\.github/workflows/qualification\.yml\s*$",
        )

    def test_readme_badges_match_trigger_ownership(self):
        text = (ROOT / "README.md").read_text()
        self.assertIn("label=Main", text)
        self.assertIn("label=Nightly", text)
        self.assertNotIn("label=Weekly", text)
        self.assertNotIn("qualification.yml?query=", text)


if __name__ == "__main__":
    unittest.main()
