"""Executable gate and artifact contract tests. Run with unittest discovery."""

import json
import os
import tempfile
import unittest
from contextlib import contextmanager
from pathlib import Path
from unittest.mock import patch

import artifact
import gate


@contextmanager
def workspace():
    previous = Path.cwd()
    with tempfile.TemporaryDirectory() as directory:
        os.chdir(directory)
        try:
            yield Path(directory)
        finally:
            os.chdir(previous)


class GateTests(unittest.TestCase):
    def test_requires_every_expected_job(self):
        for result in ("failure", "cancelled", "skipped", "timed_out", None):
            with self.subTest(result=result), self.assertRaises(ValueError):
                gate.check({"tests": {"result": result}}, ["tests"])
        with self.assertRaises(ValueError):
            gate.check({}, ["tests"])
        gate.check({"tests": {"result": "success"}, "optional": {"result": "skipped"}}, ["tests"])

    def test_optional_failure_still_fails(self):
        with self.assertRaises(ValueError):
            gate.check({"optional": {"result": "failure"}}, [])


class ArtifactTests(unittest.TestCase):
    def seed(self):
        artifact.OUT.mkdir()
        for name, target in (artifact.ARCHIVES | artifact.CONTAINERS).items():
            suffix = ".tar" if name in artifact.CONTAINERS else (".zip" if "windows" in name else ".tar.gz")
            file = artifact.OUT / (name + suffix)
            file.write_bytes(b"verified candidate")
            extra = {"image_id": "sha256:" + "a" * 64} if name in artifact.CONTAINERS else {}
            artifact.record(name, target, file, **extra)

    @patch.object(artifact, "identity", return_value=("a" * 40, "1.2.3"))
    def test_complete_set_and_each_missing_artifact(self, _identity):
        with workspace():
            self.seed()
            receipt = artifact.verify()
            self.assertEqual(len(receipt["artifacts"]), 7)
            self.assertEqual(receipt["sha"], "a" * 40)
            for name in artifact.ARCHIVES | artifact.CONTAINERS:
                record = artifact.OUT / f"{name}.json"
                original = record.read_bytes()
                record.unlink()
                with self.assertRaises(FileNotFoundError):
                    artifact.verify()
                record.write_bytes(original)

    @patch.object(artifact, "identity", return_value=("a" * 40, "1.2.3"))
    def test_rejects_wrong_commit_version_platform_path_and_bytes(self, _identity):
        with workspace():
            self.seed()
            path = artifact.OUT / "kei-linux-x86_64.json"
            original = json.loads(path.read_text())
            for field, value in [("sha", "b" * 40), ("version", "1.2.4"), ("target", "wrong"), ("file", "../outside"), ("sha256", "0" * 64)]:
                with self.subTest(field=field):
                    path.write_text(json.dumps({**original, field: value}))
                    with self.assertRaises(ValueError):
                        artifact.verify()
            path.write_text(json.dumps(original))
            (artifact.OUT / original["file"]).write_bytes(b"changed after qualification")
            with self.assertRaises(ValueError):
                artifact.verify()

    @patch.object(artifact, "identity", return_value=("a" * 40, "1.2.3"))
    def test_container_identity_is_required_before_publication(self, _identity):
        with workspace():
            self.seed()
            path = artifact.OUT / "container-amd64.json"
            item = json.loads(path.read_text())
            for value in ("", "wrong", "sha256:short"):
                path.write_text(json.dumps({**item, "image_id": value}))
                with self.assertRaises(ValueError):
                    artifact.verify()

    @patch.object(artifact, "identity", return_value=("a" * 40, "1.2.3"))
    def test_packages_and_executes_extracted_unix_binary(self, _identity):
        if os.name == "nt":
            self.skipTest("shell fixture is Unix-only")
        with workspace():
            source = Path("target/x86_64-unknown-linux-gnu/release/kei")
            source.parent.mkdir(parents=True)
            source.write_text('#!/bin/sh\ncase "$1" in --version) echo "kei 1.2.3";; --help) echo help;; config) test "$2" = show;; *) exit 1;; esac\n')
            source.chmod(0o755)
            artifact.package("x86_64-unknown-linux-gnu", "kei-linux-x86_64")
            record = json.loads((artifact.OUT / "kei-linux-x86_64.json").read_text())
            self.assertEqual(record["sha256"], artifact.digest(artifact.OUT / record["file"]))


if __name__ == "__main__":
    unittest.main()
