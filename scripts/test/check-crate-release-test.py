import hashlib
import importlib.util
import sys
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).resolve().parent.parent / "check-crate-release.py"
sys.dont_write_bytecode = True
SPEC = importlib.util.spec_from_file_location("check_crate_release", SCRIPT)
check_crate_release = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(check_crate_release)

TAG = "v0.1.0"
COMMIT = "a" * 40


class ReleaseGate(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.artifacts = Path(temporary.name)
        self.manifest = {"package": {"name": "duolane", "version": "0.1.0"}}
        self.tree = (
            b"100755 blob aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\tLICENSE\0"
            b"100644 blob bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb\tdocs/README.md\0"
        )
        self.runs = [{
            "databaseId": 101,
            "headBranch": TAG,
            "headSha": COMMIT,
            "conclusion": "success",
            "event": "push",
        }]
        self.owners = {"users": [{"kind": "user", "login": "dannyota"}]}
        self.release = {"isDraft": False, "isImmutable": False, "assets": []}
        for artifact, extension in check_crate_release.ARCHIVES.items():
            directory = self.artifacts / artifact
            directory.mkdir()
            archive = f"duolane-{artifact}.{extension}"
            checksum = f"duolane-{artifact}.sha256"
            archive_bytes = archive.encode()
            archive_hash = hashlib.sha256(archive_bytes).hexdigest()
            for name, data in ((archive, archive_bytes), (checksum, f"{archive_hash}  {archive}\n".encode())):
                (directory / name).write_bytes(data)
                self.release["assets"].append({
                    "name": name,
                    "state": "uploaded",
                    "digest": f"sha256:{hashlib.sha256(data).hexdigest()}",
                })

    def validate(self):
        return check_crate_release.validate(
            TAG, COMMIT, self.manifest, self.release, self.runs, self.owners, self.artifacts, self.tree,
        )

    def test_complete_release_passes(self):
        self.assertIsNone(self.validate())

    def test_wrong_manifest_version_fails(self):
        self.manifest["package"]["version"] = "0.1.1"
        with self.assertRaises(ValueError):
            self.validate()

    def test_package_metadata_paths_stay_inside_checkout(self):
        package = self.manifest["package"]
        package.update({"readme": "./docs//README.md", "license-file": "LICENSE"})
        self.assertIsNone(self.validate())
        for field, path in (("readme", "../README.md"), ("license-file", "/tmp/LICENSE")):
            original = package[field]
            package[field] = path
            with self.subTest(field=field):
                with self.assertRaises(ValueError):
                    self.validate()
            package[field] = original

    def test_explicit_package_metadata_requires_tracked_files(self):
        for field in ("readme", "license-file"):
            with self.subTest(field=field):
                self.manifest["package"][field] = ".git/logs/HEAD"
                with self.assertRaises(ValueError):
                    self.validate()
            del self.manifest["package"][field]

    def test_package_requires_standalone_manifest(self):
        for table, workspace in ((self.manifest, {}), (self.manifest["package"], "..")):
            table["workspace"] = workspace
            with self.subTest(workspace=workspace):
                with self.assertRaises(ValueError):
                    self.validate()
            del table["workspace"]

    def test_release_run_for_another_commit_fails(self):
        self.runs[0]["headSha"] = "b" * 40
        with self.assertRaises(ValueError):
            self.validate()

    def test_swapped_asset_digests_fail(self):
        first, second = self.release["assets"][:2]
        first["digest"], second["digest"] = second["digest"], first["digest"]
        with self.assertRaises(ValueError):
            self.validate()

    def test_team_owner_fails(self):
        self.owners["users"][0]["kind"] = "team"
        with self.assertRaises(ValueError):
            self.validate()


if __name__ == "__main__":
    unittest.main()
