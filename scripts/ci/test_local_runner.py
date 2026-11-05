"""Exercise local dispatch and evidence without credentials."""

import json
import os
import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path

import local_release

ROOT = Path(__file__).resolve().parents[2]


def executable(path, body):
    path.write_text("#!/usr/bin/env bash\nset -euo pipefail\n" + body)
    path.chmod(0o755)


class LocalRunnerTests(unittest.TestCase):
    def test_dispatch_and_finalize_without_offline_replay(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory) / "repo"
            scripts = root / "scripts/full-test"
            scripts.mkdir(parents=True)
            (root / "scripts/ci").mkdir()
            (root / "tests/shell").mkdir(parents=True)
            (root / "bin").mkdir()
            for name in ("run_local_release.sh", "begin_run.sh", "time_phase.sh"):
                shutil.copyfile(ROOT / "scripts/full-test" / name, scripts / name)
                (scripts / name).chmod(0o755)
            shutil.copyfile(ROOT / "scripts/ci/local_release.py", root / "scripts/ci/local_release.py")
            for name in ("cargo", "docker", "just"):
                executable(root / "bin" / name, 'printf "%s %s\\n" "$(basename "$0")" "$*" >> "$CALLS"\necho "test result: ok. 1 passed; 0 failed; 0 ignored;"\n')
            executable(scripts / "check_prereqs.sh", ":\n")
            executable(scripts / "run_shell_suites.sh", ":\n")
            executable(scripts / "run_live_import_rehearsal.sh", ":\n")
            executable(scripts / "run_live_smokes.sh", "\n".join(
                f"scripts/full-test/time_phase.sh --live {phase} -- true" for phase in sorted(local_release.LIVE_COMMANDS)))
            def git(*args):
                subprocess.run(["git", *args], cwd=root, check=True, capture_output=True)
            git("init", "-q")
            git("add", "scripts", "bin")
            git("-c", "user.name=CI", "-c", "user.email=ci@example.invalid", "commit", "-qm", "fixture")
            state = Path(directory) / "state"
            env = {**os.environ, "PATH": f"{root / 'bin'}:{os.environ['PATH']}",
                   "CALLS": str(Path(directory) / "calls"), "KEI_FULL_TEST_RUNS_DIR": str(state),
                   "KEI_FULLTEST_LOG_DIR": str(Path(directory) / "logs"),
                   "KEI_TEST_SCRATCH_DIR": str(Path(directory) / "shell"),
                   "KEI_FULL_TEST_TMPDIR": str(Path(directory) / "tmp"),
                   "KEI_FULL_TEST_CROSS_ZONE_ALBUM": ""}
            def run():
                return subprocess.run(["bash", str(scripts / "run_local_release.sh")], cwd=root, env=env, capture_output=True, text=True, check=False)
            result = run()
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            record = json.loads((state / "qualified.json").read_text())
            self.assertEqual(set(record["phases"]), local_release.BASE_PHASES | local_release.LIVE_COMMANDS)
            self.assertEqual(Path(env["CALLS"]).read_text().splitlines(), [
                "cargo build --locked --release", "docker build -t kei:local-release .", "just test live"])
            self.assertFalse((state / ".run-marker").exists())
            (root / "dirty").touch()
            self.assertNotEqual(run().returncode, 0)



if __name__ == "__main__":
    unittest.main()
