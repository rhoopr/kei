#!/usr/bin/env python3
"""Optional Linux qualification against caller-supplied official release assets.

No downloads. Use a disk-backed scratch directory outside /tmp. The default
offline Rust gate uses the checked-in SQL schema instead of this binary.
"""
import argparse
import hashlib
import os
import subprocess
import tarfile
from pathlib import Path

DIGEST = "0402df3eff13904ca1417d5b52758ccbe98a2d7f5360b84cb04cde5972a5c4f3"
ARCHIVE = "kei-linux-x86_64.tar.gz"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--assets", type=Path, required=True,
                        help="directory containing v0.24.0 archive and SHA256SUMS.txt")
    parser.add_argument("--scratch", type=Path, required=True,
                        help="new or existing isolated disk-backed qualification directory")
    args = parser.parse_args()
    archive = args.assets / ARCHIVE
    actual = hashlib.sha256(archive.read_bytes()).hexdigest()
    checksums = (args.assets / "SHA256SUMS.txt").read_text().splitlines()
    if actual != DIGEST:
        parser.error(f"unexpected release archive: {actual}")
    if not any(line.split() == [DIGEST, ARCHIVE] for line in checksums):
        parser.error("release checksum mismatch")
    scratch = args.scratch.resolve()
    if scratch == Path("/tmp") or Path("/tmp") in scratch.parents:
        parser.error("--scratch must be outside /tmp")
    scratch.mkdir(parents=True, exist_ok=True)
    # Extract only the expected regular binary, never arbitrary archive paths.
    binary = scratch / "kei-v0.24.0"
    with tarfile.open(archive) as release:
        members = [m for m in release.getmembers() if m.name in ("kei", "./kei") and m.isfile()]
        if len(members) != 1:
            parser.error("archive must contain exactly one kei binary")
        with release.extractfile(members[0]) as stream:
            binary.write_bytes(stream.read())
    binary.chmod(0o700)
    output = subprocess.check_output(
        ["unshare", "-Urn", str(binary), "--version"], env={"PATH": os.defpath}, text=True
    ).strip()
    if output != "kei 0.24.0":
        parser.error(f"unexpected binary version: {output}")
    temp = scratch / "tmp"
    temp.mkdir(exist_ok=True)
    env = dict(os.environ, KEI_TEST_RELEASED_V0240=str(binary), TMPDIR=str(temp))
    # Require the caller's explicit disk-backed build cache, not a /tmp build.
    target = Path(env.get("CARGO_TARGET_DIR", scratch / "target")).resolve()
    if target == Path("/tmp") or Path("/tmp") in target.parents:
        parser.error("CARGO_TARGET_DIR must be outside /tmp")
    env["CARGO_TARGET_DIR"] = str(target)
    print(f"Verified {ARCHIVE}: sha256:{actual}; {output}", flush=True)
    print("Released tag commit: aec13c42ce476edd8f8e0019bf58da1fd5d1b879", flush=True)
    for target_args in (["--test", "behavioral"], ["--lib"]):
        command = ["unshare", "-Urn", "sh", "-ec",
                   'ip link set lo up; exec "$@"', "kei-upgrade",
                   "cargo", "test", "--offline", *target_args,
                   "released_v0240_", "--", "--nocapture"]
        print("Network-disabled command:", " ".join(command), flush=True)
        subprocess.run(command, cwd=Path(__file__).resolve().parents[1], env=env, check=True)



if __name__ == "__main__":
    main()
