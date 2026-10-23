'''Validate a guarded publication resume against the original release run.'''

from __future__ import annotations

import argparse
import json
import os
import subprocess
import sys
from collections.abc import Callable, Mapping
from typing import Any

REPOSITORY = 'rhoopr/kei'
RELEASE_WORKFLOW = '.github/workflows/release.yml'
VALIDATE_JOB = 'validate'
QUALIFICATION_JOB = 'qualification / Heavy qualification result'
RELEASE_JOB = 'release'
MAX_TAG_DEPTH = 8
MAX_JOB_PAGES = 100

Api = Callable[[str], Any]


def _mapping(value: Any, what: str) -> Mapping[str, Any]:
    if not isinstance(value, Mapping):
        raise TypeError('invalid ' + what)
    return value


def _sha(value: Any) -> str:
    if (
        not isinstance(value, str)
        or len(value) != 40
        or not value.isascii()
        or any(character not in '0123456789abcdef' for character in value)
    ):
        raise ValueError('invalid commit SHA')
    return value


def _numeric(value: Any) -> bool:
    if not isinstance(value, str):
        return False
    return (
        bool(value)
        and value.isascii()
        and value.isdigit()
        and (value == '0' or value[0] != '0')
    )


def require_repository(repo: Any) -> str:
    if repo != REPOSITORY:
        raise ValueError('repository must be rhoopr/kei')
    return repo


def require_run(run_id: Any) -> str:
    if not _numeric(run_id) or run_id == '0':
        raise ValueError('run must be a positive decimal')
    return run_id


def require_tag(tag: Any) -> str:
    if not isinstance(tag, str) or not tag.startswith('v'):
        raise ValueError('tag must be vX.Y.Z')
    parts = tag[1:].split('.')
    if len(parts) != 3 or not all(_numeric(part) for part in parts):
        raise ValueError('tag must be vX.Y.Z')
    return tag


def require_release(api: Api, repo: str, tag: str) -> None:
    release = _mapping(
        api('repos/' + repo + '/releases/tags/' + tag),
        'release',
    )
    if release.get('tag_name') != tag or release.get('draft') is not False:
        raise ValueError('matching non-draft release is required')


def resolve_tag(api: Api, repo: str, tag: str) -> str:
    ref = _mapping(
        api('repos/' + repo + '/git/ref/tags/' + tag),
        'tag ref',
    )
    if ref.get('ref') not in (None, 'refs/tags/' + tag):
        raise ValueError('tag ref does not match requested tag')
    target = _mapping(ref.get('object'), 'tag target')
    seen: set[str] = set()
    for _ in range(MAX_TAG_DEPTH):
        sha = _sha(target.get('sha'))
        if sha in seen:
            raise ValueError('tag chain contains a cycle')
        seen.add(sha)
        kind = target.get('type')
        if kind == 'commit':
            return sha
        if kind != 'tag':
            raise ValueError('tag target is not a commit')
        annotated = _mapping(
            api('repos/' + repo + '/git/tags/' + sha),
            'annotated tag',
        )
        target = _mapping(annotated.get('object'), 'annotated tag target')
    raise ValueError('tag chain is too deep')


def _fetch_jobs(api: Api, repo: str, run_id: str) -> list[Mapping[str, Any]]:
    collected: list[Mapping[str, Any]] = []
    for page in range(1, MAX_JOB_PAGES + 1):
        payload = _mapping(
            api(
                'repos/' + repo + '/actions/runs/' + run_id
                + '/jobs?per_page=100&page=' + str(page)
            ),
            'jobs page',
        )
        batch = payload.get('jobs')
        if not isinstance(batch, list) or not all(isinstance(job, Mapping) for job in batch):
            raise ValueError('invalid jobs page')
        total = payload.get('total_count')
        if not isinstance(total, int) or isinstance(total, bool) or total < 0:
            raise ValueError('invalid job count')
        collected.extend(batch)
        if len(collected) == total:
            return collected
        if len(collected) > total or not batch:
            raise ValueError('job pagination mismatch')
    raise ValueError('too many job pages')


def require_jobs(jobs: list[Mapping[str, Any]]) -> None:
    for name in (VALIDATE_JOB, QUALIFICATION_JOB, RELEASE_JOB):
        matches = [job for job in jobs if job.get('name') == name]
        if len(matches) != 1:
            raise ValueError('missing or ambiguous required job: ' + name)
        job = matches[0]
        if job.get('status') != 'completed' or job.get('conclusion') != 'success':
            raise ValueError('required job did not succeed: ' + name)


def validate(repo: str, run_id: str, tag: str, api: Api) -> dict[str, str]:
    repo = require_repository(repo)
    run_id = require_run(run_id)
    tag = require_tag(tag)

    require_release(api, repo, tag)
    latest = _mapping(api('repos/' + repo + '/releases/latest'), 'latest release')
    if latest.get('tag_name') != tag:
        raise ValueError('resume must target the latest stable release')
    sha = resolve_tag(api, repo, tag)

    run = _mapping(
        api('repos/' + repo + '/actions/runs/' + run_id),
        'workflow run',
    )
    if run.get('status') != 'completed':
        raise ValueError('original run must be completed')
    if run.get('conclusion') not in ('success', 'failure'):
        raise ValueError('original run did not finish with a usable conclusion')
    if run.get('event') != 'push':
        raise ValueError('original run must be a push')
    if run.get('path') != RELEASE_WORKFLOW:
        raise ValueError('original run must use the release workflow')
    if run.get('head_branch') != tag:
        raise ValueError('original run tag does not match')
    if run.get('head_sha') != sha:
        raise ValueError('original run commit does not match tag')
    repository = _mapping(run.get('repository'), 'run repository')
    if repository.get('full_name') != repo:
        raise ValueError('original run repository does not match')

    require_jobs(_fetch_jobs(api, repo, run_id))
    return {'sha': sha, 'tag': tag, 'version': tag[1:], 'run_id': run_id}


def write_output(result: Mapping[str, Any]) -> None:
    target = os.environ.get('GITHUB_OUTPUT')
    if not target:
        raise ValueError('GITHUB_OUTPUT is required')
    sha = _sha(result.get('sha'))
    tag = require_tag(result.get('tag'))
    run_id = require_run(result.get('run_id'))
    version = result.get('version')
    if version != tag[1:]:
        raise ValueError('invalid output version')
    values = {'sha': sha, 'tag': tag, 'version': version, 'run_id': run_id}
    with open(target, 'a', encoding='utf-8') as stream:
        for key in ('sha', 'tag', 'version', 'run_id'):
            print(key + '=' + values[key], file=stream)


def gh_api(endpoint: str) -> Any:
    completed = subprocess.run(
        ['gh', 'api', endpoint],
        check=True,
        capture_output=True,
        text=True,
    )
    return json.loads(completed.stdout)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description='Validate a release publication resume')
    parser.add_argument('--repo', required=True)
    parser.add_argument('--run', required=True, dest='run_id')
    parser.add_argument('--tag', required=True)
    arguments = parser.parse_args(argv)
    try:
        result = validate(
            arguments.repo,
            arguments.run_id,
            arguments.tag,
            gh_api,
        )
        write_output(result)
    except (TypeError, ValueError, OSError, subprocess.CalledProcessError) as error:
        print(error, file=sys.stderr)
        return 1
    return 0


if __name__ == '__main__':
    raise SystemExit(main())
