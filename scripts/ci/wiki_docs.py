"""Offline, revision-scoped wiki contracts. Never evaluate documentation shell text."""

import argparse
import collections
import hashlib
import json
import re
import shlex
import subprocess
import tempfile
from pathlib import Path
from urllib.parse import unquote, urlsplit

import tomllib


class ContractError(ValueError):
    """A documentation contract does not match its pinned source."""


def git(repo, *args):
    return subprocess.check_output(["git", "-c", "core.quotepath=false", "-C", str(repo), *args], text=True,
                                   stderr=subprocess.PIPE)


def read_source(repo, revision, path):
    try:
        return git(repo, "show", f"{revision}:{path}")
    except subprocess.CalledProcessError as exc:
        raise ContractError(f"missing source: {revision}:{path}") from exc


def blocks(text):
    fence = None
    content = []
    for number, line in enumerate(text.splitlines(), 1):
        match = re.match(r"^\s*(`{3,}|~{3,})(.*)$", line)
        if match:
            marker, language = match.groups()
            if fence is None:
                fence = marker[0]
                content = []
                kind = language.strip()
                start = number
            elif marker[0] == fence:
                yield start, kind, "\n".join(content)
                fence = None
            continue
        if fence:
            content.append(line)
    if fence:
        raise ContractError("unclosed code fence")


def prose(text):
    return re.sub(r"(?ms)^\s*(`{3,}|~{3,}).*?^\s*\1\s*$", "", text)


def anchors(text):
    counts = collections.Counter()
    result = set(re.findall(r'<a\s+(?:name|id)=[\"\']([^\"\']+)', text))
    for line in prose(text).splitlines():
        match = re.match(r"^#{1,6}\s+(.*?)\s*#*\s*$", line)
        if not match:
            continue
        name = re.sub(r"\[([^]]+)\]\([^)]*\)", r"\1", match[1])
        name = re.sub(r"<[^>]+>", "", name).lower().replace(" ", "-")
        name = "".join(c for c in name if c.isalnum() or c in "_-")
        index = counts[name]
        counts[name] += 1
        result.add(name + (f"-{index}" if index else ""))
    return result


def links(text):
    text = prose(text)
    definitions = dict(re.findall(r"(?m)^\[([^]]+)\]:\s*<?([^\s>]+)>?", text))
    for match in re.finditer(r"\[[^]\n]+\]\(([^)\n]+)\)|\[\[([^]\n]+)\]\]", text):
        yield (match[1] or match[2].split("|")[-1]).strip("<>")
    for match in re.finditer(r"\[[^]\n]+\]\[([^]\n]+)\]", text):
        if match[1] not in definitions:
            raise ContractError(f"undefined link reference: {match[1]}")
        yield definitions[match[1]]
    if re.search(r"\[[^]\n]+\]\([^\n)]*$", text, re.MULTILINE):
        raise ContractError("malformed Markdown link")


def check_links(pages, repo):
    headings = {name: anchors(text) for name, text in pages.items()}
    for name, text in pages.items():
        for target in links(text):
            url = urlsplit(target)
            fragment = unquote(url.fragment)
            if url.scheme:
                if url.scheme not in {"http", "https", "mailto"} or (
                        url.scheme != "mailto" and not url.netloc):
                    raise ContractError(f"{name}: malformed URL {target}")
                if url.netloc == "github.com" and re.match(r"/rhoopr/kei/blob/(main|master)/", url.path):
                    raise ContractError(f"{name}: mutable canonical link {target}")
                match = re.fullmatch(r"/rhoopr/kei/blob/([0-9a-f]{40})/(.+)", url.path)
                if url.netloc == "github.com" and match:
                    revision, path = match.groups()
                    source = read_source(repo, revision, unquote(path))
                    line_range = re.fullmatch(r"L(\d+)(?:-L(\d+))?", fragment)
                    if line_range:
                        first, last = int(line_range[1]), int(line_range[2] or line_range[1])
                        if not 1 <= first <= last <= len(source.splitlines()):
                            raise ContractError(f"{name}: missing source line anchor {target}")
                    elif fragment:
                        if fragment not in anchors(source):
                            raise ContractError(f"{name}: missing source anchor {target}")
                continue
            page = unquote(url.path).removesuffix(".md") or name
            if page not in pages or (fragment and fragment not in headings[page]):
                raise ContractError(f"{name}: broken page/anchor {target}")


def flatten(value, prefix=""):
    for key, item in value.items():
        name = f"{prefix}.{key}" if prefix else key
        if isinstance(item, dict):
            yield from flatten(item, name)
        else:
            yield name, item


def dictionary(text):
    keys = set(re.findall(r"(?m)^# Option: (.+)$", text))
    uncommented = re.sub(r"(?m)^# ([a-z_]+\s*=)", r"\1", text)
    values = dict(flatten(tomllib.loads(uncommented)))
    return keys, values


def check_toml(text, keys, types):
    try:
        values = dict(flatten(tomllib.loads(text)))
    except tomllib.TOMLDecodeError as exc:
        raise ContractError(f"invalid TOML: {exc}") from exc
    for key, value in values.items():
        if key not in keys:
            raise ContractError(f"unknown config key: {key}")
        if key in types and type(value) is not type(types[key]):
            raise ContractError(f"wrong config type: {key}")


def struct_fields(cli, name):
    match = re.search(r"pub struct " + name + r"\s*\{(.*?)\n\}", cli, re.DOTALL)
    if not match:
        raise ContractError(f"missing CLI structure {name}")
    fields = set()
    previous = 0
    for field in re.finditer(r"(?m)^    pub ([a-z_]+):", match[1]):
        attributes = match[1][previous:field.start()]
        previous = field.end()
        for argument in re.findall(r"#\[arg\((.*?)\)\]", attributes, re.DOTALL):
            long = re.search(r'\blong(?:\s*=\s*"([^"\n]+)")?(?=\s*[,)]|\s*$)', argument)
            if long:
                fields.add(long[1] or field[1].replace("_", "-"))
    return fields


def command_surface(cli):
    global_flags = struct_fields(cli, "Cli") - {"command"} | {"help", "version"}
    password = struct_fields(cli, "PasswordArgs")
    sync = struct_fields(cli, "SyncArgs") - {"config-overrides"}
    surface = {}
    for command, owner in {
        "sync": "SyncArgs", "login": "PasswordArgs", "password": "PasswordArgs",
        "list": "PasswordArgs", "status": "StatusArgs", "doctor": "DoctorArgs",
        "support-export": "SupportExportArgs", "manifest": "ManifestArgs",
        "import-existing": "ImportArgs", "verify": "VerifyArgs", "reconcile": "ReconcileArgs",
        "migrate-state": "MigrateStateArgs", "install": "InstallArgs", "uninstall": "UninstallArgs",
        "service": "SyncArgs", "config": "Cli", "reset": "Cli",
    }.items():
        surface[command] = global_flags | struct_fields(cli, owner)
    for command in ["sync", "service"]:
        surface[command] |= password | sync | struct_fields(cli, "FriendlyArgs")
    surface["migrate-state"] |= password
    surface["list"] |= {"library"}
    surface["reset"] |= {"yes"}
    surface["config"] |= {"output"}
    return surface


def command_argv(line):
    try:
        tokens = shlex.split(line, comments=True)
    except ValueError as exc:
        raise ContractError(f"malformed shell example: {line}") from exc
    if not tokens or tokens[0].startswith("#"):
        return None
    # Only literal invocations; wrappers are parsed as words, never executed.
    if tokens[0] in {"kei", "kei.exe"}:
        return tokens[1:]
    if tokens[0] in {"sudo", "env"} and "kei" in tokens:
        return tokens[tokens.index("kei") + 1:]
    if tokens[0] == "docker":
        if not ({"run", "exec"} & set(tokens[1:3])):
            return None
        if "kei" in tokens:
            args = tokens[tokens.index("kei") + 1:]
            args = args[1:] if args[:1] == ["kei"] else args
            return args or None
        for i, token in enumerate(tokens):
            if token.startswith("ghcr.io/rhoopr/kei:"):
                return tokens[i + 1:] or None
    return None


def check_argv(argv, surface, actions, value_flags=()):
    args = list(argv)
    while args and args[0] in {"--config", "--log-level"}:
        if len(args) < 2 or args[1].startswith("--"):
            raise ContractError(f"missing value for {args[0]}")
        args = args[2:]
    if not args or args[0] in {"--version", "--help"}:
        return
    command = args[0]
    if command not in surface:
        raise ContractError(f"unknown command: {command}")
    bad = {arg.split("=", 1)[0][2:] for arg in args if arg.startswith("--")} - surface[command]
    if bad:
        raise ContractError(f"unsupported {command} flags: {sorted(bad)}")
    positional = []
    index = 1
    while index < len(args):
        item = args[index]
        flag = item.split("=", 1)[0].removeprefix("--")
        if item.startswith("--") and flag in value_flags:
            if "=" not in item:
                if index + 1 >= len(args) or args[index + 1].startswith("--"):
                    raise ContractError(f"missing value for --{flag}")
                index += 1
            elif not item.split("=", 1)[1]:
                raise ContractError(f"missing value for --{flag}")
        elif not item.startswith("-"):
            positional.append(item)
        index += 1
    if command in actions and positional and positional[0] not in actions[command]:
        raise ContractError(f"unknown {command} action: {positional[0]}")


def binary_help(binary, argv):
    with tempfile.TemporaryDirectory(prefix="kei-docs-help-") as temp:
        env = {"HOME": temp, "XDG_CONFIG_HOME": temp, "KEI_DATA_DIR": temp,
               "PATH": "/usr/bin:/bin", "LANG": "C.UTF-8"}
        result = subprocess.run([str(binary), *argv, "--help"], env=env,
                                capture_output=True, text=True, timeout=15, check=False)
    return result.returncode == 0


def validate(repo, wiki, contract, binary=None):
    pages = {p.stem: p.read_text() for p in wiki.glob("*.md")}
    if not pages:
        raise ContractError("wiki has no Markdown pages")
    if contract["schema_version"] != 1:
        raise ContractError("unsupported contract schema")
    profile = contract["stable"]
    for ref in [profile["source_revision"], contract["wiki_revision"], contract["compatibility_revision"]]:
        if not re.fullmatch(r"[0-9a-f]{40}", ref):
            raise ContractError("contract revisions must be exact commit SHAs")
    revision = profile["source_revision"]
    cargo = tomllib.loads(read_source(repo, revision, "Cargo.toml"))
    if cargo["package"]["version"] != profile["version"]:
        raise ContractError("profile version does not match pinned source")
    cli = read_source(repo, revision, "src/cli.rs")
    surface = command_surface(cli)
    value_flags = set()
    for owner in re.findall(r"pub struct ([A-Za-z]+) \{", cli):
        body = re.search(r"pub struct " + owner + r" \{(.*?)\n\}", cli, re.DOTALL)[1]
        for field, kind in re.findall(r"(?m)^    pub ([a-z_]+): ([^,\n]+)", body):
            if kind.strip() != "bool":
                value_flags.add(field.replace("_", "-"))
    for command, owner in {"login": "LoginCommand", "password": "PasswordAction",
                           "list": "ListCommand", "config": "ConfigAction",
                           "service": "ServiceAction", "reset": "ResetCommand"}.items():
        if command not in contract["actions"]:
            continue
        body = re.search(r"pub enum " + owner + r" \{(.*?)\n\}", cli, re.DOTALL)
        if not body:
            raise ContractError(f"missing action owner {owner}")
        actual = {re.sub(r"(?<!^)(?=[A-Z])", "-", name).lower()
                  for name in re.findall(r"(?m)^    ([A-Z][A-Za-z]+)(?:[, ({])", body[1])}
        if actual != set(contract["actions"][command]):
            raise ContractError(f"action contract drift: {command}")
    if git(wiki, "rev-parse", "HEAD").strip() != contract["wiki_revision"]:
        raise ContractError("wiki HEAD differs from contract revision")
    source_enum = cli.split("pub enum Command {", 1)[1].split("\n}\n", 1)[0]
    commands = [re.sub(r"(?<!^)(?=[A-Z])", "-", c).lower()
                for c in re.findall(r"(?m)^    ([A-Z][A-Za-z]+)(?:\s*\{|\()", source_enum)]
    home = pages["Home"].split("## Commands\n", 1)[1].split("\n## ", 1)[0]
    if commands != re.findall(r"(?m)^\| \[`([^`]+)`", home):
        raise ContractError("Home command order differs from pinned source")
    compatibility_files = compatibility_anchors = 0
    for path in git(wiki, "ls-tree", "-r", "--name-only", contract["compatibility_revision"]).splitlines():
        if not path.endswith(".md"):
            continue
        name = Path(path).stem
        old = read_source(wiki, contract["compatibility_revision"], path)
        compatibility_files += 1
        compatibility_anchors += len(anchors(old))
        if name not in pages or not anchors(old) <= anchors(pages[name]):
            raise ContractError(f"lost compatibility page/anchor: {path}")
    check_links(pages, repo)
    if "guide_revision" in profile:
        guides = {"backup-maintenance.md", "docker-first-backup.md", "metadata.md",
                  "migration-from-icloudpd.md"}
        for name, text in pages.items():
            for target in links(text):
                url = urlsplit(target)
                if (url.netloc == "github.com" and url.path.startswith("/rhoopr/kei/blob/")
                        and url.path.split("/")[-1] in guides
                        and profile["guide_revision"] not in url.path):
                    raise ContractError(f"{name}: stale canonical guide revision")
    legacy = {name for name, text in pages.items()
              if name.startswith("CLI") and "removed in v0.20" in text}
    if legacy:
        index = pages.get("Legacy-Flags", "")
        indexed = {unquote(urlsplit(target).path) for target in links(index)}
        if not legacy <= indexed:
            raise ContractError(f"legacy migration index misses: {sorted(legacy - indexed)}")
    keys, types = dictionary(read_source(repo, revision, "example.config.toml"))
    listed = set(re.findall(r"(?m)^\| `([^`]+)`", pages["Configuration"]))
    if keys != listed:
        raise ContractError(f"configuration dictionary keys differ: missing={sorted(keys - listed)}, extra={sorted(listed - keys)}")
    shell_count = toml_count = binary_count = 0
    exclusions = {(x["page"], x["line"]): x for x in contract["historical_examples"]}
    used = set()
    for name, text in pages.items():
        for number, language, content in blocks(text):
            if language == "toml":
                check_toml(content, keys, types)
                toml_count += 1
            if language not in {"sh", "bash", "powershell"}:
                continue
            for offset, line in enumerate(content.splitlines(), number + 1):
                argv = command_argv(line)
                if argv is None:
                    continue
                key = (name, line.strip())
                if key in exclusions:
                    used.add(key)
                    continue
                try:
                    check_argv(argv, surface, contract["actions"], value_flags)
                except ContractError as exc:
                    raise ContractError(f"{name}:{offset}: {exc}") from exc
                shell_count += 1
    if used != set(exclusions):
        raise ContractError("stale historical example exclusion")
    for fixture in contract["command_fixtures"]:
        if fixture["page"] not in pages or not fixture["without_help_effects"]:
            raise ContractError("fixture lacks page/effect provenance")
        check_argv(fixture["argv"], surface, contract["actions"], value_flags)
        if binary:
            if not binary_help(binary, fixture["argv"]):
                raise ContractError(f"binary rejected help fixture: {fixture['argv']}")
            binary_count += 1
    return {"version": profile["version"], "source_revision": revision,
            "wiki_head": git(wiki, "rev-parse", "HEAD").strip(), "pages": len(pages),
            "commands": len(commands), "config_keys": len(keys), "legacy_pages_indexed": len(legacy),
            "compatibility_files": compatibility_files, "compatibility_anchors": compatibility_anchors, "shell_examples": shell_count,
            "toml_examples": toml_count, "binary_help_fixtures": binary_count,
            "binary_sha256": hashlib.sha256(binary.read_bytes()).hexdigest() if binary else None,
            "qualification": "Source-contract checks; optional help parses do not test runtime effects or prove binary revision."}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo", type=Path, default=Path(__file__).resolve().parents[2])
    parser.add_argument("--wiki", type=Path, required=True)
    parser.add_argument("--contract", type=Path)
    parser.add_argument("--binary", type=Path)
    args = parser.parse_args()
    path = args.contract or args.repo / "docs/wiki-contract.json"
    try:
        result = validate(args.repo, args.wiki, json.loads(path.read_text()), args.binary)
    except (ContractError, KeyError, IndexError, subprocess.CalledProcessError) as exc:
        parser.exit(1, f"wiki docs check: {exc}\n")
    print(json.dumps(result, indent=2))


if __name__ == "__main__":
    main()
