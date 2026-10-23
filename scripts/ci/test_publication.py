'''Contract tests for publication resume validation.'''

import os
import tempfile
import unittest
from copy import deepcopy
from pathlib import Path
from unittest.mock import patch

import publication

SHA = 'a' * 40
TAG = 'v1.2.3'
RUN = '42'
JOBS = (
    {'name': publication.VALIDATE_JOB, 'status': 'completed', 'conclusion': 'success'},
    {'name': publication.QUALIFICATION_JOB, 'status': 'completed', 'conclusion': 'success'},
    {'name': publication.RELEASE_JOB, 'status': 'completed', 'conclusion': 'success'},
)
REF = 'repos/rhoopr/kei/git/ref/tags/' + TAG
RUN_PATH = 'repos/rhoopr/kei/actions/runs/' + RUN
PAGE = RUN_PATH + '/jobs?per_page=100&page=1'


class Api:
    def __init__(self, routes):
        self.routes = routes
        self.calls = []

    def __call__(self, path):
        self.calls.append(path)
        if path not in self.routes:
            raise AssertionError('unexpected API call: ' + path)
        return self.routes[path]


def routes():
    return {
        'repos/rhoopr/kei/releases/latest': {'tag_name': TAG},
        'repos/rhoopr/kei/releases/tags/' + TAG: {
            'tag_name': TAG,
            'draft': False,
        },
        REF: {'object': {'type': 'commit', 'sha': SHA}},
        RUN_PATH: {
            'status': 'completed',
            'conclusion': 'failure',
            'event': 'push',
            'path': publication.RELEASE_WORKFLOW,
            'head_branch': TAG,
            'head_sha': SHA,
            'repository': {'full_name': publication.REPOSITORY},
        },
        PAGE: {'total_count': len(JOBS), 'jobs': deepcopy(list(JOBS))},
    }


class PublicationTests(unittest.TestCase):
    def accept(self, data):
        return publication.validate(publication.REPOSITORY, RUN, TAG, Api(data))

    def test_accepts_failed_aggregate_and_annotated_tag_chain(self):
        data = routes()
        data[REF]['object'] = {'type': 'tag', 'sha': 'b' * 40}
        data['repos/rhoopr/kei/git/tags/' + 'b' * 40] = {
            'object': {'type': 'tag', 'sha': 'c' * 40},
        }
        data['repos/rhoopr/kei/git/tags/' + 'c' * 40] = {
            'object': {'type': 'commit', 'sha': SHA},
        }
        self.assertEqual(
            self.accept(data),
            {'sha': SHA, 'tag': TAG, 'version': '1.2.3', 'run_id': RUN},
        )

    def test_rejects_run_identity_mismatches(self):
        cases = {
            'sha': ('head_sha', 'b' * 40),
            'tag': ('head_branch', 'v1.2.4'),
            'source': ('path', '.github/workflows/other.yml'),
            'event': ('event', 'workflow_dispatch'),
            'repository': ('repository', {'full_name': 'other/kei'}),
            'status': ('status', 'in_progress'),
            'conclusion': ('conclusion', 'cancelled'),
        }
        for label, (key, value) in cases.items():
            with self.subTest(label=label):
                data = routes()
                data[RUN_PATH][key] = value
                with self.assertRaises(ValueError):
                    self.accept(data)

    def test_rejects_older_release_before_moving_stable_aliases(self):
        data = routes()
        data['repos/rhoopr/kei/releases/latest'] = {'tag_name': 'v2.0.0'}
        with self.assertRaises(ValueError):
            self.accept(data)

    def test_rejects_wrong_or_draft_release(self):
        releases = {
            'wrong tag': {'tag_name': 'v1.2.4', 'draft': False},
            'draft': {'tag_name': TAG, 'draft': True},
        }
        for label, release in releases.items():
            with self.subTest(label=label):
                data = routes()
                data['repos/rhoopr/kei/releases/tags/' + TAG] = release
                with self.assertRaises(ValueError):
                    self.accept(data)

    def test_requires_exactly_one_successful_gate(self):
        failed = deepcopy(list(JOBS))
        failed[0]['conclusion'] = 'failure'
        duplicate = deepcopy(list(JOBS))
        duplicate.append(deepcopy(JOBS[2]))
        unfinished = deepcopy(list(JOBS))
        unfinished[2]['status'] = 'in_progress'
        cases = {
            'missing qualification': [JOBS[0], JOBS[2]],
            'failed validate': failed,
            'duplicate release': duplicate,
            'unfinished release': unfinished,
        }
        for label, jobs in cases.items():
            with self.subTest(label=label):
                data = routes()
                data[PAGE] = {'total_count': len(jobs), 'jobs': deepcopy(jobs)}
                with self.assertRaises(ValueError):
                    self.accept(data)

    def test_paginates_jobs(self):
        data = routes()
        data[PAGE] = {'total_count': 3, 'jobs': deepcopy([JOBS[0]])}
        data[RUN_PATH + '/jobs?per_page=100&page=2'] = {
            'total_count': 3,
            'jobs': deepcopy([JOBS[1], JOBS[2]]),
        }
        self.accept(data)

    def test_rejects_unsafe_inputs_before_network(self):
        cases = (
            ('repo', '', RUN, TAG),
            ('repo', 'rhoopr/kei' + chr(10), RUN, TAG),
            ('repo', 'other/kei', RUN, TAG),
            ('run', publication.REPOSITORY, '0', TAG),
            ('run', publication.REPOSITORY, '01', TAG),
            ('run', publication.REPOSITORY, '1.0', TAG),
            ('run', publication.REPOSITORY, '../1', TAG),
            ('run', publication.REPOSITORY, '1' + chr(10), TAG),
            ('tag', publication.REPOSITORY, RUN, '1.2.3'),
            ('tag', publication.REPOSITORY, RUN, 'v1.2'),
            ('tag', publication.REPOSITORY, RUN, 'v1.2.3-rc.1'),
            ('tag', publication.REPOSITORY, RUN, 'v01.2.3'),
            ('tag', publication.REPOSITORY, RUN, 'v1.2.3' + chr(10)),
        )
        for label, repo, run_id, tag in cases:
            with self.subTest(label=label, repo=repo, run=run_id, tag=tag):
                api = Api({})
                with self.assertRaises(ValueError):
                    publication.validate(repo, run_id, tag, api)
                self.assertEqual(api.calls, [])

    def test_writes_only_validated_outputs(self):
        with tempfile.TemporaryDirectory() as directory:
            target = Path(directory) / 'output'
            with patch.dict(os.environ, {'GITHUB_OUTPUT': str(target)}):
                publication.write_output(
                    {'sha': SHA, 'tag': TAG, 'version': '1.2.3', 'run_id': RUN}
                )
            self.assertEqual(
                target.read_text().splitlines(),
                [
                    'sha=' + SHA,
                    'tag=' + TAG,
                    'version=1.2.3',
                    'run_id=' + RUN,
                ],
            )


if __name__ == '__main__':
    unittest.main()
