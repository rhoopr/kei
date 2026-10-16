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
        else:
            raise ValueError("expected package, container, or verify")
    except (ValueError, OSError, subprocess.CalledProcessError) as error:
        sys.exit(str(error))
