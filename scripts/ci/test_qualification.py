"""Executable gate and artifact contract tests. Run with unittest discovery."""

import hashlib
import io
import json
import os
import tarfile
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


def replace_member(members, name, data):
    return [
        (member_name, data if member_name == name else member_data)
        for member_name, member_data in members
    ]


def archive_bytes(members):
    buffer = io.BytesIO()
    with tarfile.open(fileobj=buffer, mode="w") as bundle:
        for name, data in members:
            info = tarfile.TarInfo(name)
            info.size = len(data)
            bundle.addfile(info, io.BytesIO(data))
    return buffer.getvalue()


def image_members(
    *,
    config_name="config.json",
    layer_names=("layer-0/layer.tar", "layer-1/layer.tar"),
    layer_bytes=(b"layer-one", b"layer-two"),
    architecture="amd64",
    os_name="linux",
    diff_ids=None,
    tags=("kei:candidate",),
    manifest=None,
    extra=(),
):
    if diff_ids is None:
        diff_ids = [
            "sha256:" + hashlib.sha256(data).hexdigest()
            for data in layer_bytes
        ]
    config = json.dumps({
        "os": os_name,
        "architecture": architecture,
        "rootfs": {"type": "layers", "diff_ids": diff_ids},
    }).encode()
    image_id = "sha256:" + hashlib.sha256(config).hexdigest()
    if manifest is None:
        manifest = [{
            "Config": config_name,
            "RepoTags": list(tags),
            "Layers": list(layer_names),
        }]
    members = [(config_name, config)]
    members.append(("manifest.json", json.dumps(manifest).encode()))
    members.extend(extra)
    for name, data in zip(layer_names, layer_bytes):
        members.append((name, data))
    return members, image_id


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

    def seed_container(self, members, image_id):
        artifact.OUT.mkdir(exist_ok=True)
        archive = artifact.OUT / "container-amd64.tar"
        archive.write_bytes(archive_bytes(members))
        artifact.record("container-amd64", "linux/amd64", archive, image_id=image_id)

    def save_fake(self, members):
        def fake(args, check=True, **kwargs):
            self.assertEqual(args[:3], ["docker", "save", "--output"])
            self.assertEqual(args[4], "kei:candidate")
            self.assertTrue(check)
            Path(args[3]).write_bytes(archive_bytes(members))

        return fake

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

    @patch.object(artifact, "identity", return_value=("a" * 40, "1.2.3"))
    def test_verify_loaded_container_accepts_backend_independent_layout(self, _identity):
        with workspace():
            original, image_id = image_members()
            self.seed_container(original, image_id)
            fresh, fresh_id = image_members(
                config_name="blobs/sha256/config",
                layer_names=("blobs/sha256/layer-1", "blobs/sha256/layer-2"),
            )
            self.assertEqual(fresh_id, image_id)
            with (
                patch.object(artifact, "run", side_effect=AssertionError("docker inspect must not run")),
                patch.object(artifact.subprocess, "run", side_effect=self.save_fake(fresh)) as save,
            ):
                artifact.verify_loaded_container("amd64")
            save.assert_called_once()

    @patch.object(artifact, "identity", return_value=("a" * 40, "1.2.3"))
    def test_verify_loaded_container_rejects_config_mutation(self, _identity):
        with workspace():
            members, image_id = image_members()
            config = next(data for name, data in members if name == "config.json")
            mutated = replace_member(members, "config.json", config + b" ")
            self.seed_container(mutated, image_id)
            with patch.object(artifact.subprocess, "run", side_effect=AssertionError("docker save must not run")), self.assertRaises(ValueError):
                artifact.verify_loaded_container("amd64")

    @patch.object(artifact, "identity", return_value=("a" * 40, "1.2.3"))
    def test_verify_loaded_container_rejects_fresh_archive_mutation(self, _identity):
        with workspace():
            original, image_id = image_members()
            self.seed_container(original, image_id)
            fresh, _ = image_members()
            fresh = replace_member(fresh, "layer-0/layer.tar", b"changed")
            with patch.object(artifact.subprocess, "run", side_effect=self.save_fake(fresh)), self.assertRaises(ValueError):
                artifact.verify_loaded_container("amd64")

    @patch.object(artifact, "identity", return_value=("a" * 40, "1.2.3"))
    def test_verify_loaded_container_rejects_layer_mutation_and_order(self, _identity):
        with workspace():
            members, image_id = image_members()
            mutated = replace_member(members, "layer-0/layer.tar", b"mutated layer")
            self.seed_container(mutated, image_id)
            with patch.object(artifact.subprocess, "run", side_effect=AssertionError("docker save must not run")), self.assertRaises(ValueError):
                artifact.verify_loaded_container("amd64")
        with workspace():
            reordered = [{
                "Config": "config.json",
                "RepoTags": ["kei:candidate"],
                "Layers": ["layer-1/layer.tar", "layer-0/layer.tar"],
            }]
            members, image_id = image_members(manifest=reordered)
            self.seed_container(members, image_id)
            with patch.object(artifact.subprocess, "run", side_effect=AssertionError("docker save must not run")), self.assertRaises(ValueError):
                artifact.verify_loaded_container("amd64")

    @patch.object(artifact, "identity", return_value=("a" * 40, "1.2.3"))
    def test_verify_loaded_container_rejects_provenance_and_checksum(self, _identity):
        with workspace():
            members, image_id = image_members()
            self.seed_container(members, image_id)
            record = artifact.OUT / "container-amd64.json"
            original = json.loads(record.read_text())
            for field, value in (
                ("sha", "b" * 40),
                ("version", "9.9.9"),
                ("target", "linux/arm64"),
                ("file", "other.tar"),
                ("sha256", "0" * 64),
                ("image_id", "sha256:short"),
            ):
                with self.subTest(field=field):
                    record.write_text(json.dumps({**original, field: value}))
                    with patch.object(artifact.subprocess, "run", side_effect=AssertionError("docker save must not run")), self.assertRaises(ValueError):
                        artifact.verify_loaded_container("amd64")
            record.write_text(json.dumps(original))

    @patch.object(artifact, "identity", return_value=("a" * 40, "1.2.3"))
    def test_verify_loaded_container_rejects_multi_image_manifest(self, _identity):
        with workspace():
            manifest = [
                {"Config": "config.json", "RepoTags": ["kei:candidate"], "Layers": ["layer-0/layer.tar"]},
                {"Config": "config.json", "RepoTags": ["kei:candidate"], "Layers": ["layer-0/layer.tar"]},
            ]
            members, image_id = image_members(
                layer_names=("layer-0/layer.tar",),
                layer_bytes=(b"layer-one",),
                manifest=manifest,
            )
            self.seed_container(members, image_id)
            with patch.object(artifact.subprocess, "run", side_effect=AssertionError("docker save must not run")), self.assertRaises(ValueError):
                artifact.verify_loaded_container("amd64")

    @patch.object(artifact, "identity", return_value=("a" * 40, "1.2.3"))
    def test_verify_loaded_container_rejects_duplicate_members(self, _identity):
        with workspace():
            members, image_id = image_members()
            members.append(("manifest.json", b"{}"))
            self.seed_container(members, image_id)
            with patch.object(artifact.subprocess, "run", side_effect=AssertionError("docker save must not run")), self.assertRaises(ValueError):
                artifact.verify_loaded_container("amd64")

    @patch.object(artifact, "identity", return_value=("a" * 40, "1.2.3"))
    def test_verify_loaded_container_rejects_bad_architecture(self, _identity):
        with workspace():
            members, image_id = image_members(architecture="arm64")
            self.seed_container(members, image_id)
            with patch.object(artifact.subprocess, "run", side_effect=AssertionError("docker save must not run")), self.assertRaises(ValueError):
                artifact.verify_loaded_container("amd64")
            with self.assertRaises(ValueError):
                artifact.verify_loaded_container("riscv64")

    @patch.object(artifact, "identity", return_value=("a" * 40, "1.2.3"))
    def test_verify_loaded_container_rejects_invalid_layer_digest(self, _identity):
        with workspace():
            members, image_id = image_members(diff_ids=["not-a-digest", "sha256:" + "0" * 64])
            self.seed_container(members, image_id)
            with patch.object(artifact.subprocess, "run", side_effect=AssertionError("docker save must not run")), self.assertRaises(ValueError):
                artifact.verify_loaded_container("amd64")

    @patch.object(artifact, "identity", return_value=("a" * 40, "1.2.3"))
    def test_verify_loaded_container_rejects_compressed_layers(self, _identity):
        with workspace():
            compressed = bytes.fromhex("1f8b") + b"unsupported gzip layer"
            members, image_id = image_members(
                layer_names=("layer-0/layer.tar",),
                layer_bytes=(compressed,),
            )
            self.seed_container(members, image_id)
            with patch.object(artifact.subprocess, "run", side_effect=AssertionError("docker save must not run")), self.assertRaises(ValueError):
                artifact.verify_loaded_container("amd64")


if __name__ == "__main__":
    unittest.main()
