#!/usr/bin/env bash
set -euo pipefail

if [[ $# -lt 2 || $# -gt 3 ]]; then
    echo "usage: $0 <source-repository> <wiki-directory> [contract-json]" >&2
    exit 2
fi

python3 - "$@" <<'PY'
import difflib
import hashlib
import json
import os
import re
import shlex
import subprocess
import sys
import tempfile
import tomllib
from pathlib import Path


def require(condition, message):
    if not condition:
        raise ValueError(message)


def run(argv, cwd=None, **kwargs):
    return subprocess.check_output(argv, cwd=cwd, text=True, timeout=kwargs.pop("timeout", 60), **kwargs).strip()


def git(repo, *args):
    return run(["git", "-C", str(repo), *args])


def check_source(source, profile):
    actual = git(source, "rev-parse", "HEAD")
    require(actual == profile["source_revision"], f"checkout SHA differs from profile {profile['heading']}: {actual}")
    require(not git(source, "status", "--porcelain"), "source checkout is not clean")
    version = tomllib.loads((source / "Cargo.toml").read_text())["package"]["version"]
    require(version == profile["version"], f"profile version differs from pinned source {profile['heading']}: {version}")


def main():
    repo, wiki = (Path(p).resolve() for p in sys.argv[1:3])
    contract = Path(sys.argv[3]) if len(sys.argv) == 4 else repo / "docs/wiki-contract.json"
    profiles = json.loads(contract.read_text())["live_home_profiles"]
    require(profiles and len({p["heading"] for p in profiles}) == len(profiles), "missing or duplicate profiles")
    wiki_sha = git(wiki, "rev-parse", "HEAD")
    print(f"wiki command check: wiki_sha={wiki_sha}", flush=True)
    require(not git(wiki, "status", "--porcelain"), "wiki checkout is not clean")
    home = (wiki / "Home.md").read_text()
    sections = re.findall(r"(?m)^## ([^\n]+)\n([\s\S]*?)(?=^## |\Z)", home)
    headings = [heading for heading, _ in sections]
    table_headings = {heading for heading, body in sections if re.search(r"(?m)^\| Command \|", body)}
    require(table_headings == {p["heading"] for p in profiles}, "Home command table headings differ from profiles")
    for profile in profiles:
        heading = profile["heading"]
        require(headings.count(heading) == 1, f"missing or duplicate heading: {heading}")
        body = dict(sections)[heading]
        tables = list(re.finditer(r"(?m)^\| Command \|[^\n]*\n", body))
        require(len(tables) == 1 and tables[0][0].strip() == "| Command | Description |", f"unsupported command table header: {heading}")
        intro = body[:tables[0].start()]
        # Link destinations identify guides, not the table's claimed CLI version.
        claims = re.sub(r"\]\([^\n)]*\)", "]", heading + "\n" + intro)
        # Read complete tokens regardless of Markdown delimiters. Bare integers are not version claims.
        tokens = re.findall(r"(?<![0-9A-Za-z])[vV]?[0-9](?:[0-9A-Za-z_.+-]*[0-9A-Za-z_])?", claims)
        versions = {token[1:] if token[0].lower() == "v" else token for token in tokens
                    if token[0].lower() == "v" or "." in token}
        require(versions == {profile["version"]}, f"Home version claim differs from profile: {heading}")
        require(re.fullmatch(r"[0-9a-f]{40}", profile["source_revision"]), "source revision must be an exact SHA")
        rows = body[tables[0].end():].splitlines()
        require(rows and re.fullmatch(r"\| *:?-{3,}:? *\| *:?-{3,}:? *\|", rows[0]), f"unsupported command table separator: {heading}")
        profile["commands"] = []
        for row in rows[1:]:
            if not row.strip():
                break
            command = re.fullmatch(r"\| \[`([a-z][a-z-]*)`\]\([^|)]+\) \| [^|]* \|", row)
            require(command, f"unsupported command table row: {heading}: {row}")
            profile["commands"].append(command[1])
        require(profile["commands"], f"empty command table: {heading}")
    examples = [shlex.split(line[4:]) for block in re.findall(r"(?m)^```(?:sh|bash)\n([\s\S]*?)^```$", home)
                for line in block.splitlines() if line.startswith("kei ")]
    require(examples, "Home.md has no command examples")
    with tempfile.TemporaryDirectory(prefix="kei-wiki-commands-", dir=os.environ.get("RUNNER_TEMP")) as temporary:
        work = Path(temporary)
        sources = []
        # Validate every source before any build. Never reuse an existing checkout.
        for index, profile in enumerate(profiles):
            revision = profile["source_revision"]
            print(f"wiki command check: heading={profile['heading']} version={profile['version']} source_sha={revision}", flush=True)
            if subprocess.run(["git", "-C", str(repo), "cat-file", "-e", f"{revision}^{{commit}}"],
                              capture_output=True, timeout=60, check=False).returncode:
                git(repo, "fetch", "--no-tags", "origin", revision)
            source = work / f"source-{index}"
            source.mkdir()
            git(source, "init", "--quiet")
            git(source, "fetch", "--quiet", "--depth=1", "--no-tags", str(repo), revision)
            git(source, "checkout", "--quiet", "--detach", revision)
            check_source(source, profile)
            sources.append(source)
        for profile, source in zip(profiles, sources, strict=True):
            check_source(source, profile)
            subprocess.run(["cargo", "build", "--locked", "--bin", "kei", "--target-dir", str(work / "target")],
                           cwd=source, timeout=1200, check=True)
            check_source(source, profile)
            binary = work / "target/debug/kei"
            env = {"HOME": str(work), "XDG_CONFIG_HOME": str(work), "KEI_DATA_DIR": str(work),
                   "PATH": "/usr/bin:/bin", "LANG": "C.UTF-8"}
            require(run([str(binary), "--version"], env=env, timeout=15) == f"kei {profile['version']}",
                    f"binary version differs from profile: {profile['heading']}")
            help_text = run([str(binary), "--help"], env=env, timeout=15)
            commands = re.findall(r"(?m)^  ([a-z][a-z-]*)  .+", help_text.split("Commands:\n", 1)[1].split("Options:\n", 1)[0])
            commands = [command for command in commands if command != "help"]
            require(commands, "binary has an empty command inventory")
            if commands != profile["commands"]:
                difference = "\n".join(difflib.unified_diff(commands, profile["commands"], fromfile="kei --help", tofile=profile["heading"], lineterm=""))
                raise ValueError(f"Home command table differs from pinned binary: {profile['heading']}\n{difference}")
            # Home's shell examples apply to both published profiles. Never try another binary on failure.
            for argv in examples:
                try:
                    run([str(binary), *argv, "--help"], env=env, timeout=15, stderr=subprocess.PIPE)
                except subprocess.CalledProcessError as exc:
                    raise ValueError(f"invalid Home.md example for {profile['version']}: kei {shlex.join(argv)}") from exc
            print(json.dumps({"heading": profile["heading"], "version": profile["version"],
                              "source_sha": profile["source_revision"], "wiki_sha": wiki_sha,
                              "binary_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
                              "commands": len(commands), "examples": len(examples)}), flush=True)
        require(git(wiki, "rev-parse", "HEAD") == wiki_sha and not git(wiki, "status", "--porcelain"),
                "wiki changed during command check")


try:
    main()
except (ValueError, KeyError, IndexError, OSError, subprocess.SubprocessError) as error:
    sys.exit(f"wiki command check: {error}")
PY
