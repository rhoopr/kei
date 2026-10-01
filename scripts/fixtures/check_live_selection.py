"""Reject the retired named-album environment contract in executable test code."""

import pathlib
import sys


def violations(root):
    key = "KEI_TEST_" + "ALBUM"
    paths = [root / "justfile"]
    for directory in ("tests", "scripts"):
        paths.extend(
            path
            for path in (root / directory).rglob("*")
            if path.suffix in (".rs", ".sh", ".py")
        )
    return [
        f"{path.relative_to(root)}:{number}: retired named-album consumer"
        for path in sorted(paths)
        for number, line in enumerate(path.read_text().splitlines(), 1)
        if key in line and not line.lstrip().startswith(("#", "//"))
    ]


if __name__ == "__main__":
    root = pathlib.Path(sys.argv[1]) if len(sys.argv) > 1 else pathlib.Path.cwd()
    found = violations(root)
    if found:
        print("\n".join(found), file=sys.stderr)
        sys.exit(1)
