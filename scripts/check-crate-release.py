import hashlib
import json
import re
import sys
import tomllib
from pathlib import Path

TAG = re.compile(r"v(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)")
COMMIT = re.compile(r"[0-9a-f]{40}")
CRATE = "duolane"
OWNER = "dannyota"
ARCHIVES = {"darwin": "tar.gz", "linux": "tar.gz", "windows": "zip"}


def records(value):
    if not isinstance(value, list) or not all(isinstance(item, dict) for item in value):
        raise ValueError("invalid metadata records")
    return value


def file_digest(directory: Path, name: str) -> str:
    path = directory / name
    if directory.is_symlink() or path.is_symlink() or not path.is_file():
        raise ValueError("missing artifact file")
    try:
        with path.open("rb") as stream:
            return "sha256:" + hashlib.file_digest(stream, "sha256").hexdigest()
    except OSError as error:
        raise ValueError("unreadable artifact file") from error


def validate(tag: str, commit: str, manifest: dict, release: dict, runs: list,
             owners: dict, artifacts_dir: Path) -> None:
    if not isinstance(tag, str) or not TAG.fullmatch(tag):
        raise ValueError("invalid tag")
    if not isinstance(commit, str) or not COMMIT.fullmatch(commit):
        raise ValueError("invalid commit")
    package = manifest.get("package") if isinstance(manifest, dict) else None
    if not isinstance(package, dict) or package.get("name") != CRATE or package.get("version") != tag[1:]:
        raise ValueError("manifest does not match tag")

    if len(records(runs)) != 1:
        raise ValueError("expected one release run")
    run = runs[0]
    run_id = run.get("databaseId")
    if (type(run_id) is not int or run_id <= 0 or run.get("headBranch") != tag
            or run.get("headSha") != commit or run.get("conclusion") != "success" or run.get("event") != "push"):
        raise ValueError("release run does not match tag and commit")

    users = records(owners.get("users") if isinstance(owners, dict) else None)
    if not any(user.get("kind") == "user" and user.get("login") == OWNER for user in users):
        raise ValueError("required crate owner is missing")

    if not isinstance(release, dict) or release.get("isDraft") is not False:
        raise ValueError("expected a published release")
    assets = {}
    for asset in records(release.get("assets")):
        name = asset.get("name")
        if not isinstance(name, str) or name in assets:
            raise ValueError("invalid release asset name")
        assets[name] = asset
    for system, extension in ARCHIVES.items():
        for arch in ("amd64", "arm64"):
            artifact = f"{system}-{arch}"
            for suffix in (extension, "sha256"):
                name = f"{CRATE}-{artifact}.{suffix}"
                asset = assets.get(name, {})
                if asset.get("state") != "uploaded" or asset.get("digest") != file_digest(artifacts_dir / artifact, name):
                    raise ValueError("release asset does not match build artifact")


def main(args):
    try:
        tag, commit, manifest, release, runs, owners, artifacts_dir = args
        validate(
            tag, commit,
            tomllib.loads(Path(manifest).read_text(encoding="utf-8")),
            json.loads(Path(release).read_bytes()),
            json.loads(Path(runs).read_bytes()),
            json.loads(Path(owners).read_bytes()),
            Path(artifacts_dir),
        )
    except (OSError, ValueError):
        print("check-crate-release: invalid release metadata or artifacts", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
