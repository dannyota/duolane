import importlib.util
import os
from pathlib import Path
import re
import subprocess
import sys
import time
import tomllib

spec = importlib.util.spec_from_file_location("tested_tree", Path(__file__).with_name("check-tested-tree.py"))
tested = importlib.util.module_from_spec(spec)
spec.loader.exec_module(tested)


def version_pair(read):
    manifest = tomllib.loads(read("Cargo.toml"))
    lock = tomllib.loads(read("Cargo.lock"))
    package = manifest["package"]
    matches = [p for p in lock["package"] if p.get("name") == "duolane"]
    if package.get("name") != "duolane" or len(matches) != 1 or "source" in matches[0]:
        raise ValueError("invalid package")
    version = package["version"]
    pattern = r"(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(?:-((?:0|[1-9][0-9]*|[0-9A-Za-z-]*[A-Za-z-][0-9A-Za-z-]*)(?:\.(?:0|[1-9][0-9]*|[0-9A-Za-z-]*[A-Za-z-][0-9A-Za-z-]*))*))?(?:\+[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?"
    match = re.fullmatch(pattern, version) if isinstance(version, str) else None
    if not match or any(int(part) > 2**64 - 1 for part in match.group(1, 2, 3)) or matches[0].get("version") != version:
        raise ValueError("invalid version")
    return manifest, lock, matches[0], version


def same_toml(left, right):
    if type(left) is not type(right):
        return False
    if isinstance(left, dict):
        return left.keys() == right.keys() and all(same_toml(left[key], right[key]) for key in left)
    if isinstance(left, list):
        return len(left) == len(right) and all(same_toml(a, b) for a, b in zip(left, right))
    return left == right


def classify(changes, read_old, read_new):
    try:
        fields = changes.split(b"\0")
        if not changes or fields.pop() != b"":
            return False
        changed = set()
        while fields:
            header = fields.pop(0).decode("ascii")
            match = re.fullmatch(r":([0-7]{6}) ([0-7]{6}) ([0-9a-f]{4,64}) ([0-9a-f]{4,64}) ([AMDR](?:[0-9]{1,3})?)", header)
            if not match:
                return False
            old_mode, new_mode, old_id, new_id, status = match.groups()
            kind = status[0]
            if (len(old_id) != len(new_id) or (int(old_id, 16) == 0) != (old_mode == "000000")
                    or (int(new_id, 16) == 0) != (new_mode == "000000")):
                return False
            if (kind != "R" and status != kind) or (kind == "R" and (not status[1:] or int(status[1:]) > 100)):
                return False
            if (old_mode, new_mode) not in {"A": {("000000", "100644")}, "D": {("100644", "000000")}, "M": {("100644", "100644")}, "R": {("100644", "100644")}}[kind]:
                return False
            paths = [fields.pop(0).decode("utf-8") for _ in range(2 if kind == "R" else 1)]
            for path in paths:
                if not (path in {"AGENTS.md", "CHANGELOG.md", "CLAUDE.md", "CONTRIBUTING.md", "README.md", "SECURITY.md", "Cargo.toml", "Cargo.lock"} or re.fullmatch(r"(?:docs|instructions)/.+\.md", path)):
                    return False
                changed.add(path)
        if changed & {"Cargo.toml", "Cargo.lock"}:
            old_manifest, old_lock, old_package, old_version = version_pair(read_old)
            new_manifest, new_lock, new_package, _ = version_pair(read_new)
            new_manifest["package"]["version"] = old_version
            new_package["version"] = old_version
            if not same_toml(old_manifest, new_manifest) or not same_toml(old_lock, new_lock):
                return False
        return True
    except Exception:
        return False


def wait_for_parent(repo, sha, fetch_runs, fetch_jobs, fetch_run, clock=time.monotonic, sleep=time.sleep, deadline=None):
    try:
        return tested.wait_for_release(repo, sha, fetch_runs, fetch_jobs, fetch_run, clock, sleep, deadline)
    except Exception:
        return False


def main():
    if len(sys.argv) != 1:
        return 2
    answer = False
    try:
        repo = os.environ["GITHUB_REPOSITORY"]
        sha = os.environ["GITHUB_SHA"]
        if not re.fullmatch(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", repo) or not tested.valid_sha(sha):
            raise ValueError("invalid input")
        deadline = time.monotonic() + 300
        def git(*args):
            return subprocess.run(["git", *args], check=True, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, timeout=30).stdout
        parent = git("rev-parse", "HEAD^1").decode().strip()
        if git("rev-parse", "HEAD").decode().strip() != sha or not tested.valid_sha(parent):
            raise ValueError("invalid checkout")
        changes = git("diff", "--raw", "-z", "-M", "HEAD^1", "HEAD", "--")
        def reader(rev):
            return lambda path: git("show", f"{rev}:{path}").decode("utf-8")
        if classify(changes, reader("HEAD^1"), reader("HEAD")):
            root = f"repos/{repo}"
            runs_path = f"{root}/actions/workflows/ci.yml/runs?head_sha={parent}&event=push&per_page=100"
            answer = wait_for_parent(repo, parent,
                lambda: {"workflow_runs": tested.pages(runs_path, "workflow_runs", deadline)},
                lambda run: {"jobs": tested.pages(f"{root}/actions/runs/{run['id']}/attempts/{run['run_attempt']}/jobs?per_page=100", "jobs", deadline)},
                lambda run: tested.api(f"{root}/actions/runs/{run['id']}", deadline), deadline=deadline)
    except Exception:
        pass
    print(f"skip={str(answer).lower()}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
