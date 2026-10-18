"""Exercise checked-in admission expressions, reusable routes and real shell gates."""

import json
import os
import re
import shutil
import subprocess
import tempfile
import textwrap
import unittest
from pathlib import Path

from policy import route

ROOT = Path(__file__).resolve().parents[2]


def job(workflow: str, name: str) -> str:
    jobs = (ROOT / '.github/workflows' / workflow).read_text().split('jobs:\n', 1)[1]
    return re.split(r'^  [\w-]+:\n', jobs.split(f'  {name}:\n', 1)[1], maxsplit=1, flags=re.MULTILINE)[0]


def field(body: str, name: str) -> str:
    return re.search(rf'^    {name}: (.+)$', body, re.MULTILINE)[1]


def evaluate(expression: str, *, event='pull_request', action='synchronize', draft=False,
             label='', base_changed=False, mode='', service='', run_id=91):
    # Evaluate only expressions used by ci.yml. actionlint checks GitHub syntax;
    # these tests exercise the actual checked-in strings against the event matrix.
    if '${{' in expression:
        expression = re.search(r'\$\{\{ (.+) \}\}', expression)[1]
    values = {
        'github.event.pull_request.number': 42,
        'github.event.pull_request.draft': draft,
        'github.event.changes.base': base_changed,
        'github.event.label.name': label.lower(),
        'github.event.action': action,
        'github.event_name': event,
        'github.run_id': run_id,
        'github.ref': 'refs/heads/main',
        'needs.plan.outputs.qualification': mode,
        'needs.plan.outputs.service': service,
    }
    for key, value in values.items():
        expression = expression.replace(key, repr(value))
    expression = expression.replace('||', ' or ').replace('&&', ' and ')
    expression = re.sub(r'!(?!=)', ' not ', expression)
    return eval(expression, {'__builtins__': {}}, {
        'fromJSON': json.loads, 'contains': lambda values, item: item in values,
        'format': lambda value, *args: value.format(*args), 'always': lambda: True,
    })


class DraftPRTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.ci = (ROOT / '.github/workflows/ci.yml').read_text()
        cls.reusable = (ROOT / '.github/workflows/service-smoke.yml').read_text()
        cls.plan = job('ci.yml', 'plan')
        cls.checks = job('ci.yml', 'checks')
        cls.service = job('ci.yml', 'service')
        cls.qualification = job('ci.yml', 'qualification')
        cls.aggregate = job('ci.yml', 'ci')
        cls.ignored = re.search(r'^  CI_IGNORED: (.+)$', cls.ci, re.MULTILINE)[1]
        cls.group = re.search(r'^  group: (.+)$', cls.ci, re.MULTILINE)[1]
        cls.scripts = [textwrap.dedent(body) for body in re.findall(
            r'^        run: \|\n((?:^          .*\n|^\n)+)', cls.aggregate, re.MULTILINE)]

    def run_gate(self, draft: bool, mode: str, results: dict, service='false'):
        script = self.scripts[0] if draft else self.scripts[1]
        return subprocess.run(['bash', '-e', '-c', script], cwd=ROOT,
                              env={**os.environ, 'QUALIFICATION': mode, 'SERVICE': service,
                                   'NEEDS': json.dumps(results)},
                              capture_output=True, text=True, check=False)

    def test_entry_points_and_required_marker(self):
        self.assertEqual(field(self.aggregate, 'if'), 'always()')
        self.assertIn('needs: [plan, checks, service, qualification]', self.aggregate)
        self.assertEqual(len(self.scripts), 2)
        self.assertIn("if: env.CI_IGNORED != 'true' && github.event_name == 'pull_request' && github.event.pull_request.draft", self.aggregate)
        self.assertLess(self.aggregate.index('name: Defer validation for draft PRs'),
                        self.aggregate.index('uses: actions/checkout@'))
        events = re.search(r'  pull_request:\n    types: \[([^\]]+)\]', self.ci)[1]
        for event in ('opened', 'synchronize', 'reopened', 'ready_for_review',
                      'converted_to_draft', 'labeled', 'unlabeled', 'edited'):
            self.assertIn(event, [x.strip() for x in events.split(',')])
        self.assertIn('cancel-in-progress: true', self.ci)
        self.assertIn('  push:\n    branches: [main]', self.ci)
        self.assertIn('  workflow_dispatch:', self.ci)
        self.assertNotRegex(self.ci, r'(?m)^\s+ref:', 'checkout must keep the event merge SHA')
        # Metadata has no checkout or gate and cannot emit a check named CI.
        self.assertIn("if: env.CI_IGNORED != 'true'\n        with:", self.aggregate)
        self.assertIn("routed work\n        if: env.CI_IGNORED != 'true'", self.aggregate)
        self.assertNotIn('  pull_request:', self.reusable)
        self.assertIn('  workflow_call:', self.reusable)

    def test_actual_event_expressions_isolate_metadata_and_admit_retargets(self):
        cases = [(a, '', False, False) for a in ('opened', 'synchronize', 'reopened',
                                               'ready_for_review', 'converted_to_draft')]
        cases += [('labeled', 'ci:full', False, False), ('unlabeled', 'CI:FULL', False, False),
                  ('labeled', 'bug', False, True), ('unlabeled', '@infra', False, True),
                  ('edited', '', False, True), ('edited', '', True, False)]
        for draft in (False, True):
            for action, label, changed, ignored in cases:
                args = {'action': action, 'label': label, 'base_changed': changed, 'draft': draft}
                with self.subTest(**args):
                    self.assertEqual(bool(evaluate(self.ignored, **args)), ignored)
                    self.assertEqual(bool(evaluate(field(self.plan, 'if'), **args)), not ignored and not draft)
                    self.assertEqual(evaluate(field(self.aggregate, 'name'), **args),
                                     'CI metadata (ignored)' if ignored else 'CI')
                    key = evaluate(self.group, **args)
                    if ignored:
                        self.assertEqual(key, 'ignored-91')
                        self.assertNotEqual(key, evaluate(self.group, run_id=92, **args))
                    else:
                        self.assertEqual(key, 42)
        # Unrelated events cannot cancel an active validation group even in the
        # same-head ready/draft sequence. GitHub scheduling order is not emulated.
        active = evaluate(self.group)
        for action in ('labeled', 'unlabeled', 'edited'):
            self.assertNotEqual(evaluate(self.group, action=action, label='bug'), active)

    def test_draft_ready_sequence_and_missing_routed_work(self):
        for mode, selected, other in (('false', 'checks', 'qualification'),
                                      ('true', 'qualification', 'checks')):
            for draft in (True, False, True, False):
                needs = {'plan': {'result': 'skipped' if draft else 'success'},
                         selected: {'result': 'skipped'}, other: {'result': 'skipped'},
                         'service': {'result': 'skipped'}}
                result = self.run_gate(draft, mode, needs)
                self.assertNotEqual(result.returncode, 0)
                if draft:
                    self.assertIn('mark ready for review', result.stdout)
                else:
                    needs[selected]['result'] = 'success'
                    self.assertEqual(self.run_gate(False, mode, needs).returncode, 0)

    def test_gate_rejects_each_selected_service_or_quality_failure(self):
        for mode, service, selected in [('false', 'false', ['plan', 'checks']),
                                        ('false', 'true', ['plan', 'checks', 'service']),
                                        ('true', 'true', ['plan', 'qualification'])]:
            needs = {name: {'result': 'success' if name in selected else 'skipped'}
                     for name in ('plan', 'checks', 'service', 'qualification')}
            self.assertEqual(self.run_gate(False, mode, needs, service).returncode, 0)
            for name in selected:
                for outcome in (None, 'failure', 'cancelled', 'skipped', 'timed_out'):
                    with self.subTest(mode=mode, service=service, failed=name, outcome=outcome):
                        changed = {key: dict(value) for key, value in needs.items()}
                        if outcome is None:
                            del changed[name]
                        else:
                            changed[name]['result'] = outcome
                        self.assertNotEqual(self.run_gate(False, mode, changed, service).returncode, 0)
            # Unexpected optional failures also fail closed.
            for name in set(needs) - set(selected):
                self.assertNotEqual(self.run_gate(False, mode,
                    {**needs, name: {'result': 'failure'}}, service).returncode, 0)
        for mode, service in [('', 'false'), ('false', ''), ('false', 'invalid')]:
            self.assertNotEqual(self.run_gate(False, mode, {}, service).returncode, 0)

    def test_event_change_matrix_keeps_service_once_and_heavy_stages(self):
        paths = ['README.md', 'docs/architecture.md', 'scripts/check-contracts',
                 'src/auth/session.rs', 'src/download/file/publication.rs', 'src/service/linux.rs',
                 'src/commands/service/types.rs', 'src/inventory_diagnostic/tests.rs',
                 'tests/cli.rs', 'tests/data/media-manifest.json', 'Cargo.toml', 'build.rs',
                 'Dockerfile', '.github/workflows/release.yml', '.github/workflows/ci.yml',
                 '.github/workflows/checks.yml', 'scripts/ci/nightly.py',
                 '.github/workflows/opencode-review.yml', 'fuzz/fuzz_targets/heif_rewrite.rs',
                 'unknown-build-input']
        events = [('pull_request', 'opened', '', False),
                  ('pull_request', 'synchronize', '', False),
                  ('pull_request', 'ready_for_review', '', False),
                  ('pull_request', 'converted_to_draft', '', False),
                  ('pull_request', 'labeled', 'bug', False),
                  ('pull_request', 'unlabeled', 'bug', False),
                  ('pull_request', 'labeled', 'ci:full', False),
                  ('pull_request', 'unlabeled', 'ci:full', False),
                  ('pull_request', 'edited', '', False),
                  ('pull_request', 'edited', '', True),
                  ('push', '', '', False), ('workflow_dispatch', '', '', False)]
        for path in paths:
            for event, action, label, changed in events:
                for draft in (False, True):
                    for light in (False, True):
                        args = {'event': event, 'action': action, 'label': label,
                                'base_changed': changed, 'draft': draft}
                        plan_runs = bool(evaluate(field(self.plan, 'if'), **args))
                        full = action == 'labeled' and label == 'ci:full'
                        result = route([path] if event == 'pull_request' else [], event, False, light, full)
                        outputs = dict(args, mode=json.dumps(result['qualification']),
                                       service=json.dumps(result['service']))
                        quality = plan_runs and bool(evaluate(field(self.checks, 'if'), **outputs))
                        heavy = plan_runs and bool(evaluate(field(self.qualification, 'if'), **outputs))
                        native = plan_runs and bool(evaluate(field(self.service, 'if'), **outputs))
                        with self.subTest(path=path, **args, light=light):
                            self.assertEqual(int(quality) + int(heavy), int(plan_runs))
                            self.assertEqual(int(native) + int(heavy), int(plan_runs and (result['service'] or heavy)))
                            self.assertFalse(native and heavy)
                            if event != 'pull_request':
                                self.assertTrue(plan_runs)
                                for key in ('code', 'platform', 'stress'):
                                    self.assertTrue(result[key])
                            if heavy:
                                checks = route([path], event, True, light, full)
                                for key in ('code', 'platform', 'stress', 'coverage', 'unused', 'service'):
                                    self.assertTrue(checks[key])
        for event in ('schedule', 'push', 'workflow_dispatch'):
            self.assertTrue(route([], event, True, True, False)['qualification'])

    def test_contract_catalog_is_validated_without_rust_builds(self):
        hardening = job('checks.yml', 'workflow-hardening')
        self.assertNotIn('    if:', hardening)
        self.assertIn('run: python3 scripts/check-contracts', hardening)
        self.assertFalse(route(['docs/architecture.md'], 'pull_request', False, True, False)['code'])
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for name in ('scripts', 'docs', 'src'):
                (root / name).mkdir()
            checker = root / 'scripts/check-contracts'
            shutil.copyfile(ROOT / 'scripts/check-contracts', checker)
            catalog = root / 'docs/architecture.md'
            catalog.write_text('| `SAFE_MEDIA` | synthetic contract |\n')
            (root / 'src/owner.rs').write_text('// CONTRACT: SAFE_MEDIA\nfn contract_safe_media() {}\n')
            command = ['python3', str(checker)]
            self.assertEqual(subprocess.run(command, capture_output=True, check=False).returncode, 0)
            catalog.write_text('| `MISSING_OWNER` | invalid synthetic contract |\n')
            result = subprocess.run(command, capture_output=True, text=True, check=False)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn('documented contract has no production owner marker', result.stderr)


if __name__ == '__main__':
    unittest.main()
