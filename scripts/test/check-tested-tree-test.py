import copy
import re
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
COMPARE = {"status": "ahead", "behind_by": 0, "merge_base_commit": {"sha": SHA}}


class ProofTests(unittest.TestCase):
    def test_release_proof(self):
        self.assertTrue(m.release_proof(REPO, SHA, RUN, {"jobs": [JOB]}, REF, COMPARE))
        self.assertTrue(m.release_proof(REPO, SHA, RUN, {"jobs": [JOB]}, REF, dict(COMPARE, status="identical")))
        for jobs in ({"jobs": [JOB, JOB]}, {"jobs": [JOB], "total_count": 2}, {"jobs": []}, {"jobs": [dict(JOB, run_attempt=1)]}, {"jobs": [dict(JOB, run_id=11)]}, {"jobs": [dict(JOB, conclusion="failure")]}, {"jobs": [dict(JOB, status="in_progress")]}, {}, None, {"jobs": [None]}):
            self.assertFalse(m.release_proof(REPO, SHA, RUN, jobs, REF, COMPARE))
        for run in (dict(RUN, event="pull_request"), dict(RUN, conclusion="cancelled"), dict(RUN, run_attempt=None), {}, None):
            self.assertFalse(m.release_proof(REPO, SHA, run, {"jobs": [JOB]}, REF, COMPARE))
        for ref in (dict(REF, object={"type": "tag", "sha": MASTER}), {}, None):
            self.assertFalse(m.release_proof(REPO, SHA, RUN, {"jobs": [JOB]}, ref, COMPARE))
        for compare in ({"status": "behind"}, {"status": "diverged"}, dict(COMPARE, behind_by=1), dict(COMPARE, merge_base_commit={"sha": MASTER}), {}, None):
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

    def test_deadline_between_calls(self):
        clock = [0]
        def runs():
            clock[0] = 11
            return {"workflow_runs": [RUN]}
        with patch.object(m.time, "monotonic", lambda: clock[0]):
            self.assertFalse(m.check_release(REPO, SHA, runs, lambda run: {"jobs": [JOB]}, lambda: REF, lambda sha: COMPARE, deadline=10))

    def test_pages(self):
        with patch.object(m, "command", return_value='{"total_count":0,"jobs":[]}'):
            self.assertEqual(m.pages("jobs?per_page=100", "jobs"), [])
        for malformed in ("null", "[]", '{}', '{"jobs":[]}', '{"total_count":true,"jobs":[]}', '{"total_count":-1,"jobs":[]}', '{"total_count":2,"jobs":[{}]}', "not json"):
            with patch.object(m, "command", return_value=malformed):
                with self.assertRaises((ValueError, KeyError, TypeError)):
                    m.pages("jobs?per_page=100", "jobs")
        for second in ({"jobs": [JOB]}, {"total_count": 102, "jobs": [JOB]}):
            with patch.object(m, "api", side_effect=[{"total_count": 101, "jobs": [JOB] * 100}, second]):
                with self.assertRaises((ValueError, KeyError)):
                    m.pages("jobs?per_page=100", "jobs")
        with patch.object(m, "api", side_effect=[{"total_count": 101, "jobs": [JOB] * 100}, {"total_count": 101, "jobs": [JOB]}]):
            self.assertEqual(len(m.pages("jobs?per_page=100", "jobs")), 101)
        clock = [0]
        def first_page(*args, **kwargs):
            clock[0] = 11
            return m.subprocess.CompletedProcess(args, 0, stdout=b'{"total_count":101,"jobs":[' + b'{},' * 99 + b'{}]}')
        with patch.object(m.time, "monotonic", lambda: clock[0]), patch.object(m.subprocess, "run", side_effect=first_page) as run:
            with self.assertRaises(TimeoutError):
                m.pages("jobs?per_page=100", "jobs", deadline=10)
            self.assertEqual(run.call_count, 1)

    def test_workflow_structure(self):
        root = Path(__file__).parents[2] / ".github" / "workflows"
        def jobs(name):
            text = (root / name).read_text()
            lines = text.splitlines()
            start = next(i for i, line in enumerate(lines) if line.strip() == "jobs:")
            section = lines[start + 1:]
            headers = [(i, len(match.group(1)), match.group(2)) for i, line in enumerate(section)
                       if (match := re.fullmatch(r"(\s+)([\w-]+):\s*", line))]
            indent = min(depth for _, depth, _ in headers)
            headers = [(i, name) for i, depth, name in headers if depth == indent]
            return {name: "\n".join(section[index + 1:headers[n + 1][0] if n + 1 < len(headers) else len(section)])
                    for n, (index, name) in enumerate(headers)}
        ci = jobs("ci.yml")
        for name, block in ci.items():
            if name != "ci":
                self.assertNotIn("always()", block, name)
        release = jobs("release.yml")
        for name in ("checks", "build", "smoke-windows", "release"):
            block = release[name]
            needs = re.search(r"(?m)^(\s+)needs:\s*(.+)$", block)
            self.assertIsNotNone(needs, name)
            condition = re.search(r"(?m)^" + re.escape(needs.group(1)) + r"if:\s*(.+)$", block)
            self.assertIsNotNone(condition, name)
            self.assertIn("!cancelled()", condition.group(1), name)
            for dependency in re.findall(r"[\w-]+", needs.group(2)):
                self.assertIn(f"needs.{dependency}.result", condition.group(1), name)
        block = release["tested"]
        steps = list(re.finditer(r"(?m)^(\s*)- (?:uses|id):\s*(\S+)", block))
        self.assertEqual(len(steps), 2)
        for index, step in enumerate(steps):
            body = block[step.start():steps[index + 1].start() if index + 1 < len(steps) else len(block)]
            self.assertRegex(body, r"(?m)^\s*continue-on-error:\s*true\s*$")
            if index == 0:
                self.assertRegex(body, r"(?m)^\s*id:\s*checkout\s*$")
                self.assertRegex(body, r"(?m)^\s*timeout-minutes:\s*1\s*$")
            if step.group(2) == "proof":
                self.assertRegex(body, r"(?m)^\s*timeout-minutes:\s*6\s*$")
                self.assertRegex(body, r"(?m)^\s*if:\s*\$\{\{\s*steps\.checkout\.outcome\s*==\s*'success'\s*\}\}\s*$")
        self.assertEqual(steps[1].group(2), "proof")

    def test_structure_rejects_missing_lines(self):
        root = Path(__file__).parents[2] / ".github" / "workflows"
        ci = (root / "ci.yml").read_text()
        release = (root / "release.yml").read_text()
        lines = release.splitlines(keepends=True)
        targets = [i for i, line in enumerate(lines) if line.lstrip().startswith("if: ${{ !cancelled() && needs.") or "needs.tested.result != 'success'" in line or line.strip() == "continue-on-error: true" or line.strip() in ("timeout-minutes: 6", "timeout-minutes: 1", "id: checkout", "if: ${{ steps.checkout.outcome == 'success' }}")]
        self.assertEqual(len(targets), 10)
        for index in targets:
            changed = "".join(lines[:index] + lines[index + 1:])
            def read(path, *args, **kwargs):
                return ci if path.name == "ci.yml" else changed
            with self.subTest(line=lines[index]), patch.object(Path, "read_text", autospec=True, side_effect=read):
                with self.assertRaises(AssertionError):
                    self.test_workflow_structure()


if __name__ == "__main__":
    unittest.main()
