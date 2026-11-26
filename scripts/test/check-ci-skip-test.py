import os
import subprocess
import tempfile
from unittest.mock import patch
import io
import contextlib
import importlib.util
from pathlib import Path
import sys
import unittest

sys.dont_write_bytecode = True
spec = importlib.util.spec_from_file_location("ci_skip", Path(__file__).parents[1] / "check-ci-skip.py")
m = importlib.util.module_from_spec(spec)
spec.loader.exec_module(m)


def change(path, status="M", old="100644", new="100644", other=None):
    old_id = "0" * 40 if old == "000000" else "a" * 40
    new_id = "0" * 40 if new == "000000" else "b" * 40
    return f":{old} {new} {old_id} {new_id} {status}\0{path}\0".encode() + (other.encode() + b"\0" if other else b"")


def trees(version="1.2.3", extra="", entries=""):
    return {"Cargo.toml": f'[package]\nname="duolane"\nversion="{version}"\n{extra}', "Cargo.lock": f'[[package]]\nname="duolane"\nversion="{version}"\n{entries}'}


class ClassifierTests(unittest.TestCase):
    def check(self, diff, old=None, new=None):
        old, new = old or trees(), new or trees()
        return m.classify(diff, old.__getitem__, new.__getitem__)

    def test_paths_and_modes(self):
        for path in ("docs/a.md", "README.md", "instructions/a.md"):
            self.assertTrue(self.check(change(path)))
        self.assertTrue(self.check(change("docs/a.md", "D", new="000000")))
        for diff in (change("src/a.md"), change("tests/a.md"), change("docs/a.md", "R100", other="src/a.md"), change("docs/a.md", old="120000"), change("docs/a.md", new="100755"), change("docs/a.md", new="160000"), change("other"), change("docs/a.md", "R999", other="docs/b.md"), change("docs/a.md").replace(b"a" * 40, b"0" * 40), b"bad\0", change("docs/a.md")[:-1]):
            self.assertFalse(self.check(diff))

    def test_version_changes(self):
        diff = change("Cargo.toml") + change("Cargo.lock")
        old = trees()
        self.assertTrue(self.check(diff, old, trees("1.2.4")))
        for new in (dict(old, **{"Cargo.toml": trees("1.2.4")["Cargo.toml"]}), dict(trees("1.2.4"), **{"Cargo.lock": trees("1.2.5")["Cargo.lock"]}), trees("1.2.4", extra='edition="2021"'), trees("1.2.4", entries='source="registry+x"'), trees("1.2.4", entries='\n[[package]]\nname="duolane"\nversion="1.2.4"'), trees("bad"), trees("1.2.4-01")):
            self.assertFalse(self.check(diff, old, new))

    def test_toml_types_and_u64_versions(self):
        diff = change("Cargo.toml") + change("Cargo.lock")
        self.assertFalse(self.check(diff, trees(extra="debug=1"), trees("1.2.4", extra="debug=true")))
        self.assertFalse(self.check(diff, trees(), trees("18446744073709551616.2.4")))
        self.assertTrue(self.check(diff, trees(), trees("18446744073709551615.2.4")))

    def test_stdout_contract(self):
        with patch.dict(os.environ, {}, clear=True), contextlib.redirect_stdout(io.StringIO()) as output:
            self.assertEqual(m.main(), 0)
        self.assertEqual(output.getvalue(), "skip=false\n")

    def test_workflow_output_contract(self):
        text = (Path(__file__).parents[2] / ".github/workflows/ci.yml").read_text()
        block = text.split("  changes:\n", 1)[1].split("\n  gates:", 1)[0]
        self.assertNotRegex(block, r"(?m)^    continue-on-error: true$")
        script = block.split("        run: |\n", 1)[1]
        script = "\n".join(line[10:] for line in script.splitlines())
        for output, status, checkout, expected in (("skip=true\n", 0, "success", "skip=true\n"), ("skip=false\n", 0, "success", "skip=false\n"), ("skip=true\n", 1, "success", "skip=false\n"), ("skip=true\nextra\n", 0, "success", "skip=false\n"), ("skip=true\n\n", 0, "success", "skip=false\n"), ("skip=true\n", 0, "failure", "skip=false\n")):
            with self.subTest(output=output, status=status, checkout=checkout), tempfile.TemporaryDirectory() as directory:
                env = {"PATH": os.defpath, "CHECKOUT_OUTCOME": checkout, "RUNNER_TEMP": directory, "GITHUB_OUTPUT": directory + "/output", "TEST_OUTPUT": output, "TEST_STATUS": str(status)}
                prefix = 'git() { return 0; }; python3() { printf "skip=true\\n" >> "$GITHUB_OUTPUT"; printf "%s" "$TEST_OUTPUT"; return "$TEST_STATUS"; };\n'
                subprocess.run(["bash", "-e", "-c", prefix + script], env=env, check=True, capture_output=True)
                self.assertEqual(Path(env["GITHUB_OUTPUT"]).read_text(), expected)

    def test_parent_proof(self):
        repo, sha = "owner/repo", "a" * 40
        run = {"id": 1, "run_attempt": 2, "status": "completed", "conclusion": "success", "head_sha": sha, "event": "push", "repository": {"full_name": repo}}
        job = {"name": "ci", "status": "completed", "conclusion": "success", "run_id": 1, "run_attempt": 2}
        clock = [0]
        def sleep(n):
            clock[0] += n
        def proof(runs, jobs):
            return m.wait_for_parent(repo, sha, runs, jobs, lambda run: run, lambda: clock[0], sleep)
        self.assertTrue(proof(lambda: {"workflow_runs": [run]}, lambda _: {"jobs": [job]}))
        self.assertFalse(proof(lambda: {"workflow_runs": [run]}, lambda _: {"jobs": [dict(job, conclusion="failure")]}))
        pending = dict(run, status="in_progress", conclusion=None)
        responses = iter([{"workflow_runs": [pending]}, {"workflow_runs": [run]}])
        self.assertTrue(proof(lambda: next(responses), lambda _: {"jobs": [job]}))
        clock[0] = 0
        self.assertFalse(proof(lambda: {"workflow_runs": [pending]}, lambda _: {}))
        self.assertEqual(clock[0], 300)


if __name__ == "__main__":
    unittest.main()
