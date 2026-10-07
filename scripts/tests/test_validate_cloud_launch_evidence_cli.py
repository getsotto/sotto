import importlib.machinery
import importlib.util
import io
import json
import sys
import tempfile
import unittest
from contextlib import redirect_stderr, redirect_stdout
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "scripts"))
import cloud_launch_evidence

LOADER = importlib.machinery.SourceFileLoader(
    "validate_cloud_launch_evidence_cli", str(ROOT / "scripts/validate-cloud-launch-evidence")
)
SPEC = importlib.util.spec_from_loader(LOADER.name, LOADER)
cli = importlib.util.module_from_spec(SPEC)
sys.modules[LOADER.name] = cli
LOADER.exec_module(cli)


def complete_record():
    app_sha = "a" * 40
    digest = "b" * 64
    return {
        "schema": cloud_launch_evidence.SCHEMA,
        "app_sha": app_sha,
        "stripe_api_version": cloud_launch_evidence.PINNED_API_VERSION,
        "captured_at": "2026-10-07T12:00:00Z",
        "requirements": [
            {"id": item, "result": "passed", "tested_sha": app_sha, "evidence_sha256": digest}
            for item in cloud_launch_evidence.REQUIRED_REQUIREMENTS
        ],
        "decisions": [
            {"id": item, "state": "resolved", "record_sha256": digest}
            for item in cloud_launch_evidence.REQUIRED_DECISIONS
        ],
        "checks": [
            {"check": item, "result": "passed", "tested_sha": app_sha, "artifact_sha256": digest}
            for item in cloud_launch_evidence.REQUIRED_CHECKS
        ],
    }


class CloudLaunchEvidenceCli(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)

    def write(self, value):
        path = self.root / "input.json"
        path.write_text(json.dumps(value), encoding="utf-8")
        return path

    def test_template_lists_every_gate_but_cannot_validate_as_written(self):
        template_path = self.root / "template.json"
        self.assertEqual(cli.main(["--template", str(template_path)]), 0)
        template = json.loads(template_path.read_text(encoding="utf-8"))
        self.assertEqual(
            [row["id"] for row in template["requirements"]],
            list(cloud_launch_evidence.REQUIRED_REQUIREMENTS),
        )
        self.assertEqual(
            [row["id"] for row in template["decisions"]],
            list(cloud_launch_evidence.REQUIRED_DECISIONS),
        )
        self.assertEqual(
            [row["check"] for row in template["checks"]],
            list(cloud_launch_evidence.REQUIRED_CHECKS),
        )
        self.assertTrue(all(row["result"] == "not_run" for row in template["requirements"]))
        with self.assertRaisesRegex(ValueError, "full lowercase commit SHA"):
            cloud_launch_evidence.validate(template_path)

    def test_template_mode_cannot_be_combined_with_validation(self):
        with self.assertRaises(SystemExit) as error:
            cli.main(["evidence.json", "--template", str(self.root / "template.json")])
        self.assertEqual(error.exception.code, 2)

    def test_complete_index_writes_report_that_is_not_authorisation(self):
        input_path = self.write(complete_record())
        output_path = self.root / "private" / "report.json"
        with redirect_stdout(io.StringIO()):
            result = cli.main([str(input_path), "--output", str(output_path)])
        report = json.loads(output_path.read_text(encoding="utf-8"))
        self.assertEqual(result, 0)
        self.assertEqual(report["status"], "evidence_complete")
        self.assertFalse(report["launch_authorised"])
        self.assertNotIn("b" * 64, output_path.read_text(encoding="utf-8"))

    def test_incomplete_index_writes_report_and_returns_failure(self):
        value = complete_record()
        value["checks"][0]["result"] = "blocked"
        input_path = self.write(value)
        output_path = self.root / "report.json"
        stderr = io.StringIO()
        with redirect_stderr(stderr):
            result = cli.main([str(input_path), "--output", str(output_path)])
        self.assertEqual(result, 1)
        self.assertEqual(json.loads(output_path.read_text(encoding="utf-8"))["status"], "incomplete")
        self.assertIn("blocked", stderr.getvalue())


if __name__ == "__main__":
    unittest.main()
