"""An exact successful attempt can replace work; an absent or skipped test cannot."""

import subprocess
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

import nightly_evidence as evidence

REPO = "example/kei"
SHA = "a" * 40
SUCCESS = {"status": "completed", "conclusion": "success"}


def record(workflow, event):
    return {**SUCCESS, "id": 10, "run_attempt": 2, "repository": {"full_name": REPO},
            "path": f".github/workflows/{workflow}", "head_branch": "main", "head_sha": SHA,
            "event": event, "html_url": "https://example.com/run/10"}


class EvidenceTests(unittest.TestCase):
    def exercise(self, mode, *, change=None, job_change=None, step_conclusion="success",
                 missing=False, api_failure=False, current="11", total=1):
        broad = mode == "main"
        workflow = "ci.yml" if broad else "nightly.yml"
        run = record(workflow, "push" if broad else "schedule") | (change or {})
        job = {**SUCCESS, "name": evidence.MAIN_JOB if broad else evidence.OPTIMIZED_JOB, "run_id": 10,
               "run_attempt": 2, "head_sha": SHA, "steps": [
                   {**SUCCESS, "name": evidence.OPTIMIZED_STEP, "conclusion": step_conclusion},
               ]} | (job_change or {})

        def read(path):
            if api_failure:
                raise subprocess.CalledProcessError(1, "gh")
            query = "event=push&per_page=1" if broad else "status=success&per_page=7"
            if path == f"repos/{REPO}/actions/workflows/{workflow}/runs?branch=main&head_sha={SHA}&{query}":
                return {"workflow_runs": [] if missing else [run]}
            self.assertEqual(path, f"repos/{REPO}/actions/runs/10/attempts/2/jobs?per_page=100")
            return {"jobs": [job], "total_count": total}

        with patch.object(evidence, "api", side_effect=read), tempfile.TemporaryDirectory() as directory:
            if broad:
                return evidence.require_main(REPO, SHA)
            output = Path(directory) / "output"
            evidence.plan_optimized(REPO, SHA, current, output)
            return output.read_text().strip()

    def test_broad_requires_latest_main_push_and_its_actual_shared_gate(self):
        self.exercise("main")
        for case in [{"missing": True}, {"change": {"status": "in_progress"}},
                     {"change": {"conclusion": "cancelled"}}, {"change": {"conclusion": "failure"}},
                     {"change": {"head_sha": "b" * 40}}, {"change": {"event": "workflow_dispatch"}},
                     {"change": {"head_branch": "feature"}}, {"job_change": {"name": "CI"}},
                     {"job_change": {"conclusion": "skipped"}}, {"job_change": {"run_attempt": 1}}]:
            with self.subTest(case=case), self.assertRaises(ValueError):
                self.exercise("main", **case)
        with self.assertRaises(subprocess.CalledProcessError):
            self.exercise("main", api_failure=True)

    def test_only_real_optimized_evidence_can_skip_execution(self):
        self.assertEqual(self.exercise("optimized"), "run=false")
        self.assertEqual(self.exercise("optimized", change={"event": "workflow_dispatch"}), "run=false")
        for case in [{"missing": True}, {"api_failure": True}, {"current": "10"}, {"total": 101},
                     {"change": {"path": ".github/workflows/pr.yml"}}, {"change": {"head_sha": "b" * 40}},
                     {"change": {"repository": {"full_name": "other/repo"}}}, {"change": {"event": "pull_request"}},
                     {"change": {"conclusion": "failure"}}, {"job_change": {"run_id": 11}},
                     {"job_change": {"run_attempt": 1}}, {"job_change": {"conclusion": "skipped"}},
                     {"job_change": {"steps": []}}, {"step_conclusion": "skipped"}, {"step_conclusion": "failure"}]:
            with self.subTest(case=case):
                self.assertEqual(self.exercise("optimized", **case), "run=true")

    def test_workflow_executes_the_evidence_bound_step_without_proof(self):
        workflow = (Path(__file__).resolve().parents[2] / ".github/workflows/nightly.yml").read_text()
        self.assertIn(f"    name: {evidence.OPTIMIZED_JOB}\n", workflow)
        self.assertIn(f"      - name: {evidence.OPTIMIZED_STEP}\n"
                      "        if: steps.evidence.outputs.run != 'false'\n"
                      "        run: cargo test --locked --release --all-features --lib", workflow)
