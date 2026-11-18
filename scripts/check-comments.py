#!/usr/bin/env -S python3 -I
# Usage: python3 -I scripts/check-comments.py
import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
ALLOWLIST = Path("scripts/comments-allowlist.txt")
RAW_STRING = re.compile(r'(?:br|cr|r)(#*)"')


def skip_quoted(text, start, quote):
    i, n = start, len(text)
    while i < n and text[i] != quote:
        i += 2 if text[i] == "\\" else 1
    return i + 1


def comment_spans(text):
    spans = []
    i, n = 0, len(text)
    while i < n:
        if text.startswith("//", i):
            end = text.find("\n", i)
            end = n if end < 0 else end
            spans.append((i, end))
            i = end
            continue
        if text.startswith("/*", i):
            depth, j = 1, i + 2
            while j < n and depth:
                if text.startswith("/*", j):
                    depth, j = depth + 1, j + 2
                elif text.startswith("*/", j):
                    depth, j = depth - 1, j + 2
                else:
                    j += 1
            spans.append((i, j))
            i = j
            continue
        raw = RAW_STRING.match(text, i)
        if raw and not (i and (text[i - 1].isalnum() or text[i - 1] == "_")):
            close = '"' + raw.group(1)
            end = text.find(close, raw.end())
            i = n if end < 0 else end + len(close)
            continue
        if text[i] == '"':
            i = skip_quoted(text, i + 1, '"')
            continue
        if text[i] == "'":
            if text.startswith("\\", i + 1):
                i = skip_quoted(text, i + 1, "'")
            elif i + 2 < n and text[i + 2] == "'":
                i += 3
            else:
                i += 1
            continue
        i += 1
    return spans


def findings(path, text):
    for start, end in comment_spans(text):
        line = text.count("\n", 0, start) + 1
        for offset, fragment in enumerate(text[start:end].split("\n")):
            fragment = fragment.strip()
            if fragment:
                yield f"{path}:{line + offset}: {fragment}"


def tracked_rust_files():
    listed = subprocess.run(
        ["git", "ls-files", "-z", "--", "*.rs"],
        cwd=ROOT, check=True, capture_output=True,
    ).stdout.decode()
    return [name for name in listed.split("\0") if name]


def load_allowlist():
    lines = (ROOT / ALLOWLIST).read_text(encoding="utf-8").splitlines()
    return {line for line in lines if line.strip() and not line.startswith("#")}


def main():
    allowed = load_allowlist()
    found = set()
    violations = []
    for name in tracked_rust_files():
        path = ROOT / name
        if not path.is_file():
            continue
        for finding in findings(name, path.read_text(encoding="utf-8")):
            found.add(finding)
            if finding not in allowed:
                violations.append(finding)
    violations.extend(f"{ALLOWLIST}: stale entry: {entry}" for entry in sorted(allowed - found))
    for violation in violations:
        print(violation)
    if violations:
        return 1
    print("check-comments: ok")
    return 0


if __name__ == "__main__":
    sys.exit(main())
