#!/usr/bin/env -S python3 -I
# Usage: python3 -I scripts/check-workflow-pins.py [--list]
import argparse
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
WORKFLOWS = Path(".github/workflows")
USE = re.compile(r"\s*(?:-\s+)?uses:.*")
PIN = re.compile(
    r"\s*(?:-\s+)?uses: ([\w.-]+/[\w.-]+)(?:/[\w./-]+)?@([0-9a-f]{40}) # v([0-9]+\.[0-9]+\.[0-9]+)\s*"
)


def action_pins(paths):
    pins = {}
    for path in paths:
        name = path.relative_to(ROOT)
        for number, line in enumerate(path.read_text(encoding="utf-8").splitlines(), 1):
            if not USE.fullmatch(line):
                continue
            match = PIN.fullmatch(line)
            if not match:
                raise ValueError(f"{name}:{number}: expected owner/repo@<40-hex SHA> # vX.Y.Z")
            repo, sha, version = match.groups()
            if pins.setdefault(repo, (sha, version)) != (sha, version):
                raise ValueError(f"{name}:{number}: {repo} has another pin in a workflow")
    return pins


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--list", action="store_true")
    args = parser.parse_args()
    paths = sorted(path for path in (ROOT / WORKFLOWS).iterdir() if path.is_file())
    try:
        pins = action_pins(paths)
    except (OSError, UnicodeDecodeError, ValueError) as error:
        print(error, file=sys.stderr)
        return 1
    if args.list:
        for repo, (sha, version) in sorted(pins.items()):
            print(repo, version, sha)
    else:
        print(f"check-workflow-pins: ok ({len(pins)} actions in {len(paths)} files)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
