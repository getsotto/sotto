"""The greet workflow's decision logic, tested under node --test.

The implementation is JavaScript because actions/github-script runs it inside
the workflow; keeping the mocked-history suite in node:test and proxying it
here means the existing unittest discovery in CI picks it up unchanged.
"""

import shutil
import subprocess
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
TEST_FILE = ROOT / ".github" / "scripts" / "greet-first-contribution.test.js"


class GreetFirstContribution(unittest.TestCase):
    def test_node_test_suite_passes(self):
        node = shutil.which("node")
        if node is None:
            self.skipTest("node is not installed")
        result = subprocess.run(
            [node, "--test", str(TEST_FILE)],
            capture_output=True,
            text=True,
            cwd=ROOT,
            timeout=120,
        )
        self.assertEqual(
            result.returncode,
            0,
            f"node --test failed:\n{result.stdout}\n{result.stderr}",
        )


if __name__ == "__main__":
    unittest.main()
