#!/usr/bin/env python3
"""Package, exercise, and hash exact-commit release candidates without publishing."""

import hashlib
import json
import os
import re
import subprocess
import sys
import tarfile
import tempfile
import zipfile
from pathlib import Path

import tomllib

ARCHIVES = {
    "kei-linux-x86_64": "x86_64-unknown-linux-gnu",
    "kei-linux-aarch64": "aarch64-unknown-linux-gnu",
    "kei-macos-x86_64": "x86_64-apple-darwin",
    "kei-macos-aarch64": "aarch64-apple-darwin",
    "kei-windows-x86_64": "x86_64-pc-windows-msvc",
}
CONTAINERS = {"container-amd64": "linux/amd64", "container-arm64": "linux/arm64"}
OUT = Path("candidate")
DIGEST_RE = re.compile(r"sha256:[0-9a-f]{64}")
MAX_JSON_BYTES = 8 * 1024 * 1024
LAYER_CHUNK = 1024 * 1024
COMPRESSED_MAGIC = (
    bytes.fromhex("1f8b"),
    bytes.fromhex("28b52ffd"),
    bytes.fromhex("fd377a585a00"),
    b"BZh",
)


def run(*args: str, **kwargs) -> str:
    return subprocess.check_output(args, text=True, **kwargs).strip()


def digest(path: Path) -> str:
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def identity() -> tuple[str, str]:
    sha = run("git", "rev-parse", "HEAD")
    if os.environ.get("GITHUB_SHA", sha) != sha:
        raise ValueError("checkout does not match triggering commit")
    return sha, tomllib.loads(Path("Cargo.toml").read_text())["package"]["version"]


def record(name: str, target: str, path: Path, **extra) -> None:
    sha, version = identity()
    (OUT / f"{name}.json").write_text(json.dumps({
        "sha": sha, "version": version, "target": target, "file": path.name,
        "sha256": digest(path), **extra,
    }, indent=2) + "\n")


def package(target: str, name: str) -> None:
    if ARCHIVES.get(name) != target:
        raise ValueError("unknown release target")
    _, version = identity()
    binary = "kei.exe" if "windows" in target else "kei"
    source = Path("target") / target / "release" / binary
    OUT.mkdir(exist_ok=True)
    archive = OUT / (name + (".zip" if "windows" in target else ".tar.gz"))
    if archive.suffix == ".zip":
        with zipfile.ZipFile(archive, "w", zipfile.ZIP_DEFLATED) as bundle:
            bundle.write(source, binary)
    else:
        with tarfile.open(archive, "w:gz") as bundle:
            bundle.add(source, arcname=binary)
    with tempfile.TemporaryDirectory() as work:
        if archive.suffix == ".zip":
            with zipfile.ZipFile(archive) as bundle:
                bundle.extractall(work)
        else:
            with tarfile.open(archive) as bundle:
                bundle.extractall(work, filter="data")
        extracted = str(Path(work) / binary)
        if run(extracted, "--version") != f"kei {version}":
            raise ValueError("extracted binary version mismatch")
        run(extracted, "--help")
        env = {**os.environ, "KEI_DATA_DIR": str(Path(work) / "data"),
               "ICLOUD_USERNAME": "release-smoke@example.invalid"}
        run(extracted, "config", "show", "--config", "example.config.toml", env=env)
        if digest(Path(extracted)) != digest(source):
            raise ValueError("packaged bytes differ from tested executable")
    record(name, target, archive)


def container(arch: str) -> None:
    name = f"container-{arch}"
    if name not in CONTAINERS:
        raise ValueError("unsupported container architecture")
    _, version = identity()
    if run("docker", "run", "--rm", "kei:candidate", "--version") != f"kei {version}":
        raise ValueError("container version mismatch")
    info = json.loads(run("docker", "image", "inspect", "kei:candidate"))[0]
    if f"{info['Os']}/{info['Architecture']}" != CONTAINERS[name]:
        raise ValueError("container architecture mismatch")
    OUT.mkdir(exist_ok=True)
    archive = OUT / f"{name}.tar"
    subprocess.run(["docker", "save", "--output", str(archive), "kei:candidate"], check=True)
    record(name, CONTAINERS[name], archive, image_id=info["Id"])


def _archive_members(tar: tarfile.TarFile) -> dict[str, tarfile.TarInfo]:
    members: dict[str, tarfile.TarInfo] = {}
    for member in tar:
        if member.issym() or member.islnk() or not (member.isreg() or member.isdir()):
            raise ValueError(f"unsupported archive member: {member.name}")
        if member.name in members:
            raise ValueError(f"duplicate archive member: {member.name}")
        members[member.name] = member
    return members


def _member(
    members: dict[str, tarfile.TarInfo],
    name: str,
    label: str,
) -> tarfile.TarInfo:
    member = members.get(name)
    if member is None:
        raise ValueError(f"missing {label}: {name}")
    if not member.isreg():
        raise ValueError(f"{label} is not a regular file: {name}")
    return member


def _member_bytes(
    tar: tarfile.TarFile,
    member: tarfile.TarInfo,
    label: str,
) -> bytes:
    if member.size < 0 or member.size > MAX_JSON_BYTES:
        raise ValueError(f"{label} size is out of bounds")
    stream = tar.extractfile(member)
    if stream is None:
        raise ValueError(f"{label} is unreadable")
    try:
        data = stream.read(MAX_JSON_BYTES + 1)
    finally:
        stream.close()
    if len(data) != member.size:
        raise ValueError(f"{label} was truncated")
    return data


def _manifest(
    tar: tarfile.TarFile,
    members: dict[str, tarfile.TarInfo],
) -> tuple[str, list[str]]:
    member = _member(members, "manifest.json", "manifest")
    data = _member_bytes(tar, member, "manifest")
    try:
        manifest = json.loads(data)
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise ValueError("manifest is not valid JSON") from error
    if not isinstance(manifest, list) or len(manifest) != 1:
        raise ValueError("manifest must describe exactly one image")
    entry = manifest[0]
    if not isinstance(entry, dict):
        raise TypeError("manifest entry is malformed")
    config = entry.get("Config")
    layers = entry.get("Layers")
    tags = entry.get("RepoTags")
    if not isinstance(config, str) or not config:
        raise ValueError("manifest config path is malformed")
    if (
        not isinstance(layers, list)
        or not all(isinstance(layer, str) and layer for layer in layers)
    ):
        raise ValueError("manifest layer paths are malformed")
    if len(set(layers)) != len(layers):
        raise ValueError("manifest repeats a layer")
    if config in layers:
        raise ValueError("manifest reuses config as a layer")
    if (
        not isinstance(tags, list)
        or not all(isinstance(tag, str) for tag in tags)
        or "kei:candidate" not in tags
    ):
        raise ValueError("manifest does not identify kei:candidate")
    return config, layers


def _layer_digest(tar: tarfile.TarFile, member: tarfile.TarInfo, label: str) -> str:
    stream = tar.extractfile(member)
    if stream is None:
        raise ValueError(f"layer is unreadable: {label}")
    hasher = hashlib.sha256()
    try:
        first = stream.read(LAYER_CHUNK)
        if first.startswith(COMPRESSED_MAGIC):
            raise ValueError(f"compressed layer is unsupported: {label}")
        hasher.update(first)
        while True:
            chunk = stream.read(LAYER_CHUNK)
            if not chunk:
                break
            hasher.update(chunk)
    finally:
        stream.close()
    return "sha256:" + hasher.hexdigest()


def _verify_config(config: object, architecture: str, layer_digests: list[str]) -> None:
    if not isinstance(config, dict):
        raise TypeError("config is malformed")
    if config.get("os") != "linux":
        raise ValueError("config operating system is unsupported")
    if config.get("architecture") != architecture:
        raise ValueError("config architecture does not match target")
    rootfs = config.get("rootfs")
    if not isinstance(rootfs, dict):
        raise TypeError("config rootfs is malformed")
    diff_ids = rootfs.get("diff_ids")
    if not isinstance(diff_ids, list) or not all(isinstance(value, str) for value in diff_ids):
        raise ValueError("config rootfs.diff_ids is malformed")
    if len(diff_ids) != len(layer_digests):
        raise ValueError("config layer count does not match archive")
    for index, (expected, actual) in enumerate(zip(diff_ids, layer_digests)):
        if DIGEST_RE.fullmatch(expected) is None:
            raise ValueError(f"invalid layer digest at index {index}")
        if expected != actual:
            raise ValueError(f"layer digest mismatch at index {index}")


def _verify_saved_archive(path: Path, image_id: str, architecture: str) -> None:
    if DIGEST_RE.fullmatch(image_id) is None:
        raise ValueError("invalid container image ID")
    try:
        with tarfile.open(path, "r:") as tar:
            members = _archive_members(tar)
            config_name, layers = _manifest(tar, members)
            config_member = _member(members, config_name, "config")
            config_bytes = _member_bytes(tar, config_member, "config")
            if "sha256:" + hashlib.sha256(config_bytes).hexdigest() != image_id:
                raise ValueError("config digest does not match recorded image ID")
            try:
                config = json.loads(config_bytes)
            except (UnicodeDecodeError, json.JSONDecodeError) as error:
                raise ValueError("config is not valid JSON") from error
            layer_digests = [
                _layer_digest(tar, _member(members, name, "layer"), name)
                for name in layers
            ]
            _verify_config(config, architecture, layer_digests)
    except tarfile.TarError as error:
        raise ValueError(f"invalid container archive: {path}") from error


def verify_loaded_container(arch: str) -> None:
    name = f"container-{arch}"
    target = CONTAINERS.get(name)
    if target is None:
        raise ValueError("unknown container architecture")
    sha, version = identity()
    record_path = OUT / f"{name}.json"
    item = json.loads(record_path.read_text())
    if not isinstance(item, dict):
        raise TypeError(f"invalid candidate record: {name}")
    filename = f"{name}.tar"
    if any(item.get(key) != value for key, value in {
        "sha": sha, "version": version, "target": target, "file": filename,
    }.items()):
        raise ValueError(f"invalid candidate identity: {name}")
    image_id = item.get("image_id")
    if not isinstance(image_id, str) or DIGEST_RE.fullmatch(image_id) is None:
        raise ValueError(f"invalid container image ID: {name}")
    archive = OUT / filename
    if item.get("sha256") != digest(archive):
        raise ValueError(f"candidate checksum mismatch: {name}")
    architecture = target.rsplit("/", maxsplit=1)[1]
    _verify_saved_archive(archive, image_id, architecture)
    with tempfile.TemporaryDirectory() as work:
        fresh = Path(work) / filename
        subprocess.run(["docker", "save", "--output", str(fresh), "kei:candidate"], check=True)
        _verify_saved_archive(fresh, image_id, architecture)


def verify(directory: Path = OUT) -> dict:
    sha, version = identity()
    records = []
    expected = ARCHIVES | CONTAINERS
    for name, target in expected.items():
        item = json.loads((directory / f"{name}.json").read_text())
        extension = ".tar" if name in CONTAINERS else (".zip" if "windows" in name else ".tar.gz")
        filename = name + extension
        if any(item.get(key) != value for key, value in {
            "sha": sha, "version": version, "target": target, "file": filename,
        }.items()):
            raise ValueError(f"invalid candidate identity: {name}")
        if item.get("sha256") != digest(directory / filename):
            raise ValueError(f"candidate checksum mismatch: {name}")
        if name in CONTAINERS and not re.fullmatch(r"sha256:[0-9a-f]{64}", item.get("image_id", "")):
            raise ValueError(f"invalid container image ID: {name}")
        records.append(item)
    receipt = {"sha": sha, "version": version, "artifacts": records}
    (directory / "qualification.json").write_text(json.dumps(receipt, indent=2) + "\n")
    return receipt


if __name__ == "__main__":
    try:
        command, *arguments = sys.argv[1:]
        if command == "package":
            package(*arguments)
        elif command == "container":
            container(*arguments)
        elif command == "verify":
            verify()
        elif command == "verify-loaded-container":
            verify_loaded_container(*arguments)
        else:
            raise ValueError("expected package, container, verify, or verify-loaded-container")
    except (TypeError, ValueError, OSError, subprocess.CalledProcessError) as error:
        sys.exit(str(error))
