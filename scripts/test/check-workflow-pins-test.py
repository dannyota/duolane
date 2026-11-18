#!/usr/bin/env -S python3 -I
# Usage: python3 -I scripts/test/check-workflow-pins-test.py
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).resolve().parent.parent / "check-workflow-pins.py"
SHA_A = "a" * 40
SHA_B = "b" * 40


class Gate(unittest.TestCase):
    def setUp(self):
        self.root = Path(tempfile.mkdtemp(prefix="duolane-check-pins."))
        self.addCleanup(shutil.rmtree, self.root)
        (self.root / "scripts").mkdir()
        (self.root / ".github/workflows").mkdir(parents=True)
        shutil.copy(SCRIPT, self.root / "scripts/check-workflow-pins.py")

    def workflow(self, name, *uses):
        steps = "".join(f"      {line}\n" for line in uses)
        text = f"name: x\njobs:\n  a:\n    steps:\n{steps}"
        (self.root / ".github/workflows" / name).write_text(text)

    def run_gate(self, *args):
        result = subprocess.run(
            [sys.executable, "-I", str(self.root / "scripts/check-workflow-pins.py"), *args],
            capture_output=True, text=True,
        )
        return result.returncode, (result.stdout + result.stderr).strip()

    def test_full_pins_pass_and_list(self):
        self.workflow("ci.yml", f"- uses: actions/checkout@{SHA_A} # v7.0.1")
        self.workflow(
            "codeql.yml",
            f"- uses: github/codeql-action/init@{SHA_B} # v4.39.3",
            f"- name: analyze\n        uses: github/codeql-action/analyze@{SHA_B} # v4.39.3",
        )
        self.assertEqual(self.run_gate(), (0, "check-workflow-pins: ok (2 actions in 2 files)"))
        self.assertEqual(self.run_gate("--list"), (0, "\n".join([
            f"actions/checkout 7.0.1 {SHA_A}",
            f"github/codeql-action 4.39.3 {SHA_B}",
        ])))

    def test_bad_pins_fail(self):
        for uses in [
            "- uses: actions/checkout@v7",
            "- uses: actions/checkout@v7.0.1 # v7.0.1",
            f"- uses: actions/checkout@{SHA_A[:39]} # v7.0.1",
            f"- uses: actions/checkout@{SHA_A}",
            f"- uses: actions/checkout@{SHA_A} # v7",
            f"- uses: actions/checkout@{SHA_A} # stable",
            f'- uses: "actions/checkout@{SHA_A}" # v7.0.1',
            "- uses: ./local-action",
            "- uses: docker://alpine:3",
        ]:
            with self.subTest(uses=uses):
                self.workflow("ci.yml", uses)
                code, output = self.run_gate()
                self.assertEqual(code, 1)
                self.assertIn(".github/workflows/ci.yml:5: expected owner/repo@", output)

    def test_every_file_is_checked(self):
        self.workflow("ci.yml", f"- uses: actions/checkout@{SHA_A} # v7.0.1")
        self.workflow("other.yaml", "- uses: actions/checkout@v7")
        code, output = self.run_gate()
        self.assertEqual(code, 1)
        self.assertIn(".github/workflows/other.yaml:5:", output)

    def test_one_action_has_one_pin(self):
        self.workflow("ci.yml", f"- uses: actions/checkout@{SHA_A} # v7.0.1")
        self.workflow("release.yml", f"- uses: actions/checkout@{SHA_B} # v7.0.0")
        self.assertEqual(self.run_gate(), (
            1, ".github/workflows/release.yml:5: actions/checkout has another pin in a workflow",
        ))


if __name__ == "__main__":
    unittest.main()
