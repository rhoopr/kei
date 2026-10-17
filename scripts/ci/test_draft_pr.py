"""Exercise checked-in workflow guards and shell gates for draft transitions."""

import json
import os
import re
import subprocess
import textwrap
import unittest
from pathlib import Path

from policy import route

ROOT = Path(__file__).resolve().parents[2]


def job(workflow: str, name: str) -> str:
    jobs = (ROOT / '.github/workflows' / workflow).read_text().split('jobs:\n', 1)[1]
    return re.split(r'^  [\w-]+:\n', jobs.split(f'  {name}:\n', 1)[1], maxsplit=1, flags=re.MULTILINE)[0]


def condition(body: str) -> str:
    return re.search(r'^    if: (.+)$', body, re.MULTILINE)[1]


def enabled(expression: str, event: str, draft: bool, mode: str = '') -> bool:
    # Only the boolean/route expressions used by these checked-in jobs. The
    # syntax itself is also checked by actionlint, not emulated by this helper.
    for key, value in {
        'github.event_name': event,
        'github.event.pull_request.draft': draft,
        'needs.plan.outputs.qualification': mode,
    }.items():
        expression = expression.replace(key, repr(value))
    expression = expression.replace('||', ' or ').replace('&&', ' and ')
    expression = re.sub(r'!(?!=)', ' not ', expression)
    return bool(eval(expression, {'__builtins__': {}}, {}))


class DraftPRTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.ci = (ROOT / '.github/workflows/ci.yml').read_text()
        cls.service = (ROOT / '.github/workflows/service-smoke.yml').read_text()
        cls.plan = job('ci.yml', 'plan')
        cls.checks = job('ci.yml', 'checks')
        cls.qualification = job('ci.yml', 'qualification')
        cls.aggregate = job('ci.yml', 'ci')
        cls.smoke = job('service-smoke.yml', 'smoke')
        cls.scripts = [textwrap.dedent(body) for body in re.findall(
            r'^        run: \|\n((?:^          .*\n|^\n)+)', cls.aggregate, re.MULTILINE)]

    def run_gate(self, draft: bool, mode: str, results: dict) -> subprocess.CompletedProcess:
        # Execute the actual draft sentinel or the actual aggregate shell body.
        # The checkout action is omitted; all gate code resolves from ROOT.
        script = self.scripts[0] if draft else self.scripts[1]
        return subprocess.run(['bash', '-e', '-c', script], cwd=ROOT,
                              env={**os.environ, 'QUALIFICATION': mode,
                                   'NEEDS': json.dumps(results)},
                              capture_output=True, text=True, check=False)

    def test_entry_points_and_required_marker(self):
        self.assertEqual(condition(self.aggregate), 'always()')
        self.assertIn('needs: [plan, checks, qualification]', self.aggregate)
        self.assertEqual(len(self.scripts), 2)
        self.assertIn("if: github.event_name == 'pull_request' && github.event.pull_request.draft",
                      self.aggregate)
        self.assertLess(self.aggregate.index('name: Defer validation for draft PRs'),
                        self.aggregate.index('uses: actions/checkout@'))
        for workflow in (self.ci, self.service):
            events = re.search(r'  pull_request:\n    types: \[([^\]]+)\]', workflow)[1]
            for event in ('opened', 'synchronize', 'reopened', 'ready_for_review',
                          'converted_to_draft'):
                self.assertIn(event, [x.strip() for x in events.split(',')])
            self.assertIn('cancel-in-progress: true', workflow)
            self.assertIn('github.event.pull_request.number || github.ref', workflow)
        self.assertIn('  push:\n    branches: [main]', self.ci)
        self.assertIn('  workflow_dispatch:', self.ci)
        # Default checkout is bound to the triggering SHA, never a mutable head
        # branch. ready_for_review therefore validates its current merge commit.
        self.assertNotRegex(self.ci, r'(?m)^\s+ref:', 'unexpected checkout ref override')

    def test_draft_events_skip_all_expensive_entry_points_even_with_full_route(self):
        for action in ('opened', 'synchronize', 'reopened', 'ready_for_review',
                       'converted_to_draft', 'labeled', 'unlabeled'):
            for mode in ('true', 'false'):
                with self.subTest(action=action, mode=mode):
                    plan_runs = enabled(condition(self.plan), 'pull_request', True)
                    self.assertFalse(plan_runs)
                    # Reusable job calls have an implicit success() dependency.
                    self.assertFalse(plan_runs and enabled(condition(self.checks),
                                                          'pull_request', True, mode))
                    self.assertFalse(plan_runs and enabled(condition(self.qualification),
                                                          'pull_request', True, mode))
                    self.assertFalse(enabled(condition(self.smoke), 'pull_request', True))
                    result = self.run_gate(True, mode, {})
                    self.assertNotEqual(result.returncode, 0)
                    self.assertIn('mark ready for review', result.stdout)

    def test_ready_draft_ready_without_a_new_commit_requires_fresh_validation(self):
        for mode, selected, other in (('false', 'checks', 'qualification'),
                                      ('true', 'qualification', 'checks')):
            with self.subTest(mode=mode):
                outcomes = []
                # Same triggering commit throughout: draft-open, ready, redraft,
                # ready again. No success is synthesized from draft skips.
                for draft in (True, False, True, False):
                    self.assertEqual(enabled(condition(self.plan), 'pull_request', draft),
                                     not draft)
                    self.assertEqual(enabled(condition(self.smoke), 'pull_request', draft),
                                     not draft)
                    self.assertEqual(enabled(condition(job('ci.yml', selected)),
                                             'pull_request', draft, mode), True)
                    self.assertFalse(enabled(condition(job('ci.yml', other)),
                                             'pull_request', draft, mode))
                    needs = {'plan': {'result': 'skipped' if draft else 'success'},
                             selected: {'result': 'skipped'}, other: {'result': 'skipped'}}
                    outcomes.append(self.run_gate(draft, mode, needs).returncode)
                    if not draft:
                        # The real gate rejects the ready run until its selected
                        # work succeeds, regardless of the earlier same-SHA run.
                        self.assertNotEqual(outcomes[-1], 0)
                        needs[selected]['result'] = 'success'
                        self.assertEqual(self.run_gate(False, mode, needs).returncode, 0)
                self.assertTrue(all(outcomes))

    def test_ready_gate_rejects_missing_failed_cancelled_and_skipped_work(self):
        for mode, selected, other in (('false', 'checks', 'qualification'),
                                      ('true', 'qualification', 'checks')):
            for failed in ('plan', selected):
                for outcome in (None, 'failure', 'cancelled', 'skipped', 'timed_out'):
                    with self.subTest(mode=mode, failed=failed, outcome=outcome):
                        needs = {'plan': {'result': 'success'}, selected: {'result': 'success'},
                                 other: {'result': 'skipped'}}
                        if outcome is None:
                            del needs[failed]
                        else:
                            needs[failed]['result'] = outcome
                        self.assertNotEqual(self.run_gate(False, mode, needs).returncode, 0)
            self.assertNotEqual(self.run_gate(False, '', {}).returncode, 0)

    def test_non_pr_callers_and_path_routes_are_preserved(self):
        for event in ('push', 'workflow_dispatch', 'schedule', 'merge_group'):
            for draft in (False, True):
                with self.subTest(event=event, draft=draft):
                    self.assertTrue(enabled(condition(self.plan), event, draft))
                    self.assertTrue(enabled(condition(self.smoke), event, draft))
                    result = route(['src/auth/session.rs'], event, False, True, False)
                    self.assertTrue(result['code'])
                    self.assertTrue(result['stress'])
                    self.assertTrue(result['platform'])
        for files, heavy, light, full in [(['README.md'], False, True, False),
                                         (['src/auth/session.rs'], False, True, False),
                                         (['src/state/db.rs'], False, True, False),
                                         (['README.md'], False, True, True)]:
            result = route(files, 'pull_request', heavy, light, full)
            mode = json.dumps(result['qualification'])
            self.assertTrue(enabled(condition(self.plan), 'pull_request', False))
            self.assertEqual(enabled(condition(self.qualification), 'pull_request', False, mode),
                             result['qualification'])
            self.assertEqual(enabled(condition(self.checks), 'pull_request', False, mode),
                             not result['qualification'])
        self.assertIn("      - 'src/service/**'", self.service)
        self.assertIn('  workflow_call:', self.service)


if __name__ == '__main__':
    unittest.main()
