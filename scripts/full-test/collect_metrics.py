#!/usr/bin/env python3
"""Collect repo-level metrics for /full-test runs.

Run from the kei repo root after the build + docker phases have completed.
The release binary under Cargo's target directory and the kei:dev image must
both exist. Emits a JSON object on stdout that finalize_run.sh embeds in the
run record.

Metrics:
- binary_mb           Release binary size, MB (1dp). Bloat detector.
- docker_image_mb     Docker image size, MB. Layer / dep bloat.
- src_unwrap_expect   Count of .unwrap()/.expect() calls in src/. Per
                      AGENTS.md, must stay near zero.
- src_allow_attrs     Count of #[allow(...)] attrs in src/. Per AGENTS.md,
                      must stay near zero (fix warnings, don't suppress).
- deps_count          Crate count in Cargo.lock. Dependency-creep proxy.
- fuzz_target_count   Number of fuzz targets under fuzz/fuzz_targets/.
"""
from __future__ import annotations

import json
import os
import re
import subprocess
import sys
from pathlib import Path


def repo_root() -> Path:
    return Path(
        subprocess.check_output(
            ["git", "rev-parse", "--show-toplevel"], text=True
        ).strip()
    )


def file_size_mb(path: Path) -> float | None:
    if not path.exists():
        return None
    return round(path.stat().st_size / 1048576, 1)


def cargo_target_dir(repo: Path) -> Path:
    target = Path(os.environ.get("CARGO_TARGET_DIR", "target"))
    return target if target.is_absolute() else repo / target


def docker_image_size_mb(tag: str) -> float | None:
    try:
        out = subprocess.check_output(
            ["docker", "image", "inspect", tag, "--format", "{{.Size}}"],
            stderr=subprocess.DEVNULL,
            text=True,
        ).strip()
        return round(int(out) / 1048576, 1)
    except (subprocess.CalledProcessError, FileNotFoundError):
        return None


def rg_count_matches(pattern: str, paths: list[str]) -> int:
    """Sum match counts from `rg --count-matches`. Returns 0 if no matches."""
    try:
        out = subprocess.check_output(
            ["rg", "--count-matches", pattern, *paths],
            stderr=subprocess.DEVNULL,
            text=True,
        )
    except subprocess.CalledProcessError as e:
        # rg exits 1 when no matches found -- treat as 0, propagate other rcs.
        if e.returncode == 1:
            return 0
        raise
    except FileNotFoundError:
        return -1  # rg unavailable; signal explicit "unknown"
    total = 0
    for line in out.splitlines():
        if ":" in line:
            try:
                total += int(line.rsplit(":", 1)[-1])
            except ValueError:
                pass
    return total


def src_unwrap_expect(repo: Path) -> int:
    """Count .unwrap()/.expect() in non-test code in src/.

    Heuristic: kei convention puts test modules at the bottom of each file
    behind `#[cfg(test)]`. Stop counting once we hit that line. This
    approximates the "no .unwrap() in prod" rule from AGENTS.md without
    needing an AST. False negative if a file has scattered cfg(test)
    items above prod code -- rare in this repo.
    """
    pat = re.compile(r"\.(?:unwrap|expect)\(")
    total = 0
    for path in (repo / "src").rglob("*.rs"):
        try:
            text = path.read_text(errors="replace")
        except OSError:
            continue
        for line in text.splitlines():
            if "#[cfg(test)]" in line:
                break
            total += len(pat.findall(line))
    return total


def src_allow_attrs(repo: Path) -> int:
    """Count #[allow(...)] attrs in non-test code in src/. Same heuristic."""
    pat = re.compile(r"#\[allow")
    total = 0
    for path in (repo / "src").rglob("*.rs"):
        try:
            text = path.read_text(errors="replace")
        except OSError:
            continue
        for line in text.splitlines():
            if "#[cfg(test)]" in line:
                break
            total += len(pat.findall(line))
    return total


def fuzz_target_count(repo: Path) -> int:
    """Count fuzz targets under fuzz/fuzz_targets/."""
    targets_dir = repo / "fuzz" / "fuzz_targets"
    if not targets_dir.is_dir():
        return 0
    return sum(1 for f in targets_dir.iterdir() if f.suffix == ".rs")


def deps_count(repo: Path) -> int | None:
    lock = repo / "Cargo.lock"
    if not lock.exists():
        return None
    return sum(
        1 for line in lock.read_text().splitlines() if line.startswith("name = ")
    )


def main() -> int:
    repo = repo_root()
    os.chdir(repo)
    metrics = {
        "binary_mb": file_size_mb(cargo_target_dir(repo) / "release" / "kei"),
        "docker_image_mb": docker_image_size_mb("kei:dev"),
        "src_unwrap_expect": src_unwrap_expect(repo),
        "src_allow_attrs": src_allow_attrs(repo),
        "deps_count": deps_count(repo),
        "fuzz_target_count": fuzz_target_count(repo),
    }
    json.dump(metrics, sys.stdout, indent=2, sort_keys=True)
    sys.stdout.write("\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
