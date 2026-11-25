import copy
import importlib.util
import sys
from pathlib import Path
import unittest
from unittest.mock import mock_open, patch

sys.dont_write_bytecode = True
spec = importlib.util.spec_from_file_location("tested_tree", Path(__file__).parents[1] / "check-tested-tree.py")
m = importlib.util.module_from_spec(spec)
spec.loader.exec_module(m)

REPO = "owner/repo"
SHA = "a" * 40
HEAD = "b" * 40
BASE = "c" * 40
TREE = "d" * 40
PATHS = {path: "e" * 40 for path in (".github", "scripts", "Makefile", ".tool-versions", "rust-toolchain.toml", ".config")}
PR = {"number": 1, "merged_at": "2026-01-01", "merge_commit_sha": SHA, "base": {"ref": "master", "repo": {"full_name": REPO}}, "head": {"sha": HEAD}}
RUN = {"id": 10, "status": "completed", "conclusion": "success", "head_sha": HEAD, "event": "pull_request", "repository": {"full_name": REPO}, "pull_requests": [{"number": 1, "base": {"ref": "master"}}]}
JOBS = {"jobs": [{"id": n, "name": name, "conclusion": "success"} for n, name in enumerate(("gates", "rust", "deny", "gitleaks"), 1)]}
PUSH = {"status": "completed", "conclusion": "success", "head_sha": SHA, "event": "push", "head_branch": "master", "repository": {"full_name": REPO}}


def proof(parents=None, before=None, after=None, pulls=None, run=None, jobs=None, compare=None, head_tree=TREE, push_tree=TREE, timeline=None):
    return m.push_proof(REPO, SHA, [BASE, HEAD] if parents is None else parents,
                        PATHS if before is None else before, PATHS if after is None else after,
                        [PR] if pulls is None else pulls, RUN if run is None else run,
                        JOBS if jobs is None else jobs, {"status": "ahead", "behind_by": 0} if compare is None else compare,
                        head_tree, push_tree, [{"event": "opened"}] if timeline is None else timeline)


class ProofTests(unittest.TestCase):
    def test_push(self):
        self.assertTrue(proof())
        for parents in ([BASE], [BASE, HEAD, SHA], [BASE, SHA]):
            self.assertFalse(proof(parents=parents))
        for path in (".github", "Makefile"):
            changed = dict(PATHS, **{path: SHA})
            self.assertFalse(proof(after=changed))
        absent = dict(PATHS, **{".config": None})
        self.assertTrue(proof(before=absent, after=absent))
        self.assertFalse(proof(pulls=[PR, PR]))
        for field, value in [("merged_at", None), ("merge_commit_sha", HEAD), ("base", {"ref": "other", "repo": {"full_name": REPO}}), ("base", {"ref": "master", "repo": {"full_name": "other/repo"}})]:
            pr = copy.deepcopy(PR)
            pr[field] = value
            self.assertFalse(proof(pulls=[pr]))
        for field, value in [("pull_requests", []), ("pull_requests", [{"number": 2, "base": {"ref": "master"}}]), ("conclusion", "failure"), ("conclusion", "cancelled")]:
            run = copy.deepcopy(RUN)
            run[field] = value
            self.assertFalse(proof(run=run))
        for conclusion in ("failure", "skipped"):
            jobs = copy.deepcopy(JOBS)
            jobs["jobs"][0]["conclusion"] = conclusion
            self.assertFalse(proof(jobs=jobs))
        self.assertFalse(proof(jobs={"jobs": JOBS["jobs"][:-1]}))
        self.assertTrue(proof(compare={"status": "identical", "behind_by": 0}))
        for compare in ({"status": "behind", "behind_by": 1}, {"status": "diverged", "behind_by": 0}, {"status": "ahead", "behind_by": 1}, {}):
            self.assertFalse(proof(compare=compare))
        self.assertFalse(proof(push_tree=SHA))
        self.assertFalse(proof(jobs={"jobs": [None]}))
        self.assertTrue(proof(timeline=[]))
        self.assertFalse(proof(timeline=[{"event": "base_ref_changed"}]))
        self.assertFalse(proof(timeline=[None]))
        self.assertFalse(proof(timeline={}))
        self.assertFalse(m.push_proof(REPO, SHA, None, None, None, None, None, None, None, None, None, None))

    def test_timeline_fetch_failure(self):
        output = mock_open()
        with patch.object(m.sys, "argv", ["check-tested-tree.py", "push"]), \
             patch.dict(m.os.environ, {"GITHUB_REPOSITORY": REPO, "GITHUB_SHA": SHA, "GITHUB_OUTPUT": "output"}), \
             patch.object(m, "git", return_value=f"{BASE} {HEAD}"), \
             patch.object(m, "pages", side_effect=[[PR], OSError("failed timeline")]) as fetch, \
             patch("builtins.open", output):
            self.assertEqual(m.main(), 0)
        self.assertEqual(fetch.call_count, 2)
        output().write.assert_called_once_with("skip=false\n")

    def test_timeline_pages(self):
        with patch.object(m, "command", return_value='[[{"event":"opened"}],[{"event":"base_ref_changed"}]]'):
            self.assertFalse(proof(timeline=m.pages("timeline")))
        with patch.object(m, "command", return_value='[[{"event":"opened"}],{}]'):
            with self.assertRaises(ValueError):
                m.pages("timeline")

    def test_release(self):
        self.assertTrue(m.release_proof(REPO, SHA, {"workflow_runs": [PUSH]}, {"status": "ahead"}))
        self.assertTrue(m.release_proof(REPO, SHA, {"workflow_runs": [PUSH]}, {"status": "identical"}))
        for status in ("behind", "diverged", None):
            self.assertFalse(m.release_proof(REPO, SHA, {"workflow_runs": [PUSH]}, {"status": status}))
        for field, value in [("event", "pull_request"), ("head_branch", "other"), ("conclusion", "failure")]:
            run = copy.deepcopy(PUSH)
            run[field] = value
            self.assertFalse(m.release_proof(REPO, SHA, {"workflow_runs": [run]}, {"status": "ahead"}))
        self.assertFalse(m.release_proof(REPO, SHA, {"workflow_runs": [None]}, {}))

    def test_poll(self):
        pending = dict(PUSH, status="in_progress", conclusion=None)
        clock = [0]
        def sleep(seconds):
            clock[0] += seconds
        responses = iter([{"workflow_runs": [pending]}, {"workflow_runs": [PUSH]}])
        self.assertTrue(m.wait_for_release(REPO, SHA, {"status": "ahead"}, lambda: next(responses), lambda: clock[0], sleep))
        self.assertEqual(clock[0], 15)
        clock[0] = 0
        self.assertFalse(m.wait_for_release(REPO, SHA, {"status": "ahead"}, lambda: {"workflow_runs": [pending]}, lambda: clock[0], sleep))
        self.assertEqual(clock[0], 300)

    def test_yaml_when_available(self):
        try:
            import yaml
        except ImportError:
            self.skipTest("PyYAML is not available")
        workflows = {}
        for name in ("ci.yml", "release.yml"):
            with (Path(__file__).parents[2] / ".github" / "workflows" / name).open() as f:
                workflows[name] = yaml.safe_load(f)
                self.assertIsInstance(workflows[name], dict)
        for workflow in workflows.values():
            for name, job in workflow["jobs"].items():
                if name != "ci":
                    self.assertNotIn("always()", job.get("if", ""), name)
        jobs = workflows["release.yml"]["jobs"]
        def needs(job):
            value = job.get("needs", [])
            return [value] if isinstance(value, str) else value
        def ancestors(name, seen):
            for dependency in needs(jobs[name]):
                if dependency not in seen:
                    seen.add(dependency)
                    ancestors(dependency, seen)
            return seen
        for name, job in jobs.items():
            if any("if" in jobs[ancestor] for ancestor in ancestors(name, set())):
                condition = job.get("if", "")
                self.assertIn("!cancelled()", condition, name)
                for dependency in needs(job):
                    self.assertIn(f"needs.{dependency}.result", condition, name)


if __name__ == "__main__":
    unittest.main()
