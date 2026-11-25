import copy
import importlib.util
import sys
from pathlib import Path
import unittest
from unittest.mock import patch

sys.dont_write_bytecode = True
spec = importlib.util.spec_from_file_location("tested_tree", Path(__file__).parents[1] / "check-tested-tree.py")
m = importlib.util.module_from_spec(spec)
spec.loader.exec_module(m)

REPO = "owner/repo"
SHA = "a" * 40
MASTER = "b" * 40
RUN = {"id": 10, "run_attempt": 2, "status": "completed", "conclusion": "success", "head_sha": SHA, "event": "push", "repository": {"full_name": REPO}}
JOB = {"name": "rust", "status": "completed", "conclusion": "success", "run_id": 10, "run_attempt": 2}
REF = {"ref": "refs/heads/master", "object": {"type": "commit", "sha": MASTER}}
COMPARE = {"status": "ahead"}


class ProofTests(unittest.TestCase):
    def test_release_proof(self):
        self.assertTrue(m.release_proof(REPO, SHA, RUN, {"jobs": [JOB]}, REF, COMPARE))
        self.assertTrue(m.release_proof(REPO, SHA, RUN, {"jobs": [JOB]}, REF, {"status": "identical"}))
        for jobs in ({"jobs": [JOB, JOB]}, {"jobs": []}, {"jobs": [dict(JOB, run_attempt=1)]}, {"jobs": [dict(JOB, run_id=11)]}, {"jobs": [dict(JOB, conclusion="failure")]}, {"jobs": [dict(JOB, status="in_progress")]}, {}, None, {"jobs": [None]}):
            self.assertFalse(m.release_proof(REPO, SHA, RUN, jobs, REF, COMPARE))
        for run in (dict(RUN, event="pull_request"), dict(RUN, conclusion="cancelled"), dict(RUN, run_attempt=None), {}, None):
            self.assertFalse(m.release_proof(REPO, SHA, run, {"jobs": [JOB]}, REF, COMPARE))
        for ref in (dict(REF, object={"type": "tag", "sha": MASTER}), {}, None):
            self.assertFalse(m.release_proof(REPO, SHA, RUN, {"jobs": [JOB]}, ref, COMPARE))
        for compare in ({"status": "behind"}, {"status": "diverged"}, {}, None):
            self.assertFalse(m.release_proof(REPO, SHA, RUN, {"jobs": [JOB]}, REF, compare))

    def test_poll(self):
        pending = dict(RUN, status="in_progress", conclusion=None)
        clock = [0]
        def sleep(seconds):
            clock[0] += seconds
        responses = iter([{"workflow_runs": [pending]}, {"workflow_runs": [RUN]}])
        self.assertTrue(m.wait_for_release(REPO, SHA, lambda: next(responses), lambda run: {"jobs": [JOB]}, lambda: clock[0], sleep))
        self.assertEqual(clock[0], 15)
        clock[0] = 0
        self.assertFalse(m.wait_for_release(REPO, SHA, lambda: {"workflow_runs": [pending]}, lambda run: {}, lambda: clock[0], sleep))
        self.assertEqual(clock[0], 300)
        self.assertFalse(m.wait_for_release(REPO, SHA, lambda: {"workflow_runs": [None]}, lambda run: {}))

    def test_lookup_order_and_failures(self):
        calls = []
        def fetch_runs():
            calls.append("runs")
            return {"workflow_runs": [RUN]}
        def fetch_jobs(run):
            calls.append("jobs")
            return {"jobs": [JOB]}
        def fetch_ref():
            calls.append("ref")
            return REF
        def fetch_compare(sha):
            calls.append(("compare", sha))
            return COMPARE
        self.assertTrue(m.check_release(REPO, SHA, fetch_runs, fetch_jobs, fetch_ref, fetch_compare))
        self.assertEqual(calls, ["runs", "jobs", "ref", ("compare", MASTER)])
        for broken in (lambda: None, lambda: {}, lambda: {"workflow_runs": [None]}):
            self.assertFalse(m.check_release(REPO, SHA, broken, fetch_jobs, fetch_ref, fetch_compare))
        self.assertFalse(m.check_release(REPO, SHA, fetch_runs, lambda run: None, fetch_ref, fetch_compare))
        def failing_jobs(run):
            raise OSError("jobs unavailable")
        self.assertFalse(m.check_release(REPO, SHA, fetch_runs, failing_jobs, fetch_ref, fetch_compare))
        self.assertFalse(m.check_release(REPO, SHA, fetch_runs, fetch_jobs, lambda: None, fetch_compare))
        self.assertFalse(m.check_release(REPO, SHA, fetch_runs, fetch_jobs, fetch_ref, lambda sha: None))

    def test_pages(self):
        with patch.object(m, "command", return_value='[{"jobs":[]},{"jobs":[]}]'):
            self.assertEqual(m.pages("jobs", "jobs"), [])
        for malformed in ("null", "[]", '[{"jobs":[]},{}]', "not json"):
            with patch.object(m, "command", return_value=malformed):
                with self.assertRaises((ValueError, KeyError, TypeError)):
                    m.pages("jobs", "jobs")

    def test_workflow_structure(self):
        import yaml
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
        self.assertTrue(jobs["tested"]["steps"])
        self.assertTrue(all(step.get("continue-on-error") is True for step in jobs["tested"]["steps"]))
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
