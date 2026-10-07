import importlib.machinery
import importlib.util
import json
import sys
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
LOADER = importlib.machinery.SourceFileLoader(
    "cloud_launch_evidence", str(ROOT / "scripts/cloud_launch_evidence.py")
)
SPEC = importlib.util.spec_from_loader(LOADER.name, LOADER)
validator = importlib.util.module_from_spec(SPEC)
sys.modules[LOADER.name] = validator
LOADER.exec_module(validator)


def complete_record():
    app_sha = "a" * 40
    digest = "b" * 64
    return {
        "schema": validator.SCHEMA,
        "app_sha": app_sha,
        "stripe_api_version": validator.PINNED_API_VERSION,
        "captured_at": "2026-10-07T12:00:00Z",
        "requirements": [
            {
                "id": requirement,
                "result": "passed",
                "tested_sha": app_sha,
                "evidence_sha256": digest,
            }
            for requirement in validator.REQUIRED_REQUIREMENTS
        ],
        "decisions": [
            {"id": decision, "state": "resolved", "record_sha256": digest}
            for decision in validator.REQUIRED_DECISIONS
        ],
        "checks": [
            {
                "check": check,
                "result": "passed",
                "tested_sha": app_sha,
                "artifact_sha256": digest,
            }
            for check in validator.REQUIRED_CHECKS
        ],
    }


class CloudLaunchEvidence(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)

    def write(self, value, name="input.json"):
        path = self.root / name
        path.write_text(json.dumps(value), encoding="utf-8")
        return path

    def write_raw(self, value, name="raw-input.json"):
        path = self.root / name
        path.write_text(value, encoding="utf-8")
        return path

    def test_complete_index_is_reported_as_evidence_not_authorisation(self):
        source = self.write(complete_record())
        report, failures = validator.validate(source)
        self.assertEqual(failures, [])
        self.assertEqual(report["status"], "evidence_complete")
        self.assertFalse(report["launch_authorised"])
        self.assertEqual(report["requirement_count"], len(validator.REQUIRED_REQUIREMENTS))
        self.assertEqual(report["decision_count"], len(validator.REQUIRED_DECISIONS))
        self.assertEqual(report["check_count"], len(validator.REQUIRED_CHECKS))

    def test_not_run_requirement_blocks_completion(self):
        value = complete_record()
        value["requirements"][0]["result"] = "not_run"
        report, failures = validator.validate(self.write(value))
        self.assertEqual(report["status"], "incomplete")
        self.assertEqual(failures, [f"{validator.REQUIRED_REQUIREMENTS[0]}: not_run"])

    def test_unresolved_decision_blocks_completion(self):
        value = complete_record()
        value["decisions"][0]["state"] = "unresolved"
        report, failures = validator.validate(self.write(value))
        self.assertEqual(failures, [f"{validator.REQUIRED_DECISIONS[0]}: unresolved"])

    def test_missing_required_check_is_rejected(self):
        value = complete_record()
        value["checks"] = value["checks"][:-1]
        with self.assertRaisesRegex(ValueError, "exactly one row"):
            validator.validate(self.write(value))

    def test_evidence_must_be_for_the_candidate_sha(self):
        value = complete_record()
        value["checks"][0]["tested_sha"] = "c" * 40
        with self.assertRaisesRegex(ValueError, "must match app_sha"):
            validator.validate(self.write(value))

    def test_non_sha_artifact_reference_is_rejected(self):
        value = complete_record()
        value["requirements"][0]["evidence_sha256"] = "customer_123"
        with self.assertRaisesRegex(ValueError, "sha256"):
            validator.validate(self.write(value))

    def test_record_requires_exact_shape(self):
        value = complete_record()
        value["customer_id"] = "cus_sensitive"
        with self.assertRaisesRegex(ValueError, "incomplete shape"):
            validator.validate(self.write(value))

    def test_duplicate_json_result_fields_are_rejected(self):
        raw = json.dumps(complete_record())
        raw = raw.replace('"result": "passed"', '"result": "failed", "result": "passed"', 1)
        with self.assertRaisesRegex(ValueError, "duplicate JSON field"):
            validator.validate(self.write_raw(raw))

    def test_wrong_stripe_api_version_is_rejected(self):
        value = complete_record()
        value["stripe_api_version"] = "2026-08-26.dahlia"
        with self.assertRaisesRegex(ValueError, "stripe_api_version"):
            validator.validate(self.write(value))

    def test_invalid_unhashable_result_is_rejected(self):
        value = complete_record()
        value["checks"][0]["result"] = ["passed"]
        with self.assertRaisesRegex(ValueError, "invalid result"):
            validator.validate(self.write(value))

    def test_sanitised_report_does_not_emit_artifact_hashes(self):
        value = complete_record()
        input_path = self.write(value)
        report, _ = validator.validate(input_path)
        rendered = json.dumps(report)
        self.assertNotIn("b" * 64, rendered)
        self.assertNotIn("record_sha256", rendered)
        self.assertIn("evidence_fingerprint", rendered)

if __name__ == "__main__":
    unittest.main()
