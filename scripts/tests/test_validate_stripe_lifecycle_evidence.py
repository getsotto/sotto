import importlib.machinery
import importlib.util
import json
import sys
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
LOADER = importlib.machinery.SourceFileLoader(
    "validate_stripe_lifecycle_evidence", str(ROOT / "scripts/validate-stripe-lifecycle-evidence")
)
SPEC = importlib.util.spec_from_loader(LOADER.name, LOADER)
validator = importlib.util.module_from_spec(SPEC)
sys.modules[LOADER.name] = validator
LOADER.exec_module(validator)


def evidence():
    return {
        "schema": validator.SCHEMA,
        "app_sha": "a" * 40,
        "api_version": validator.PINNED_API_VERSION,
        "sandbox_account_id": "acct_test_lifecycle",
        "captured_at": "2026-10-07T00:00:00Z",
        "scenarios": [
            {
                "scenario": scenario,
                "result": "passed",
                "checks": ["provider state matched application state"],
                "provider_ids": [f"id_{scenario}"],
            }
            for scenario in validator.REQUIRED_SCENARIOS
        ],
    }


class LifecycleEvidence(unittest.TestCase):
    def write(self, value):
        handle = tempfile.NamedTemporaryFile(mode="w", encoding="utf-8", delete=False)
        with handle:
            json.dump(value, handle)
        self.addCleanup(lambda: Path(handle.name).unlink(missing_ok=True))
        return handle.name

    def output(self):
        handle = tempfile.NamedTemporaryFile(mode="w", encoding="utf-8", delete=False)
        handle.close()
        self.addCleanup(lambda: Path(handle.name).unlink(missing_ok=True))
        return handle.name

    def test_complete_run_is_sanitised(self):
        input_path = self.write(evidence())
        output_path = self.output()
        report, failures = validator.validate(input_path)
        Path(output_path).write_text(json.dumps(report), encoding="utf-8")
        self.assertEqual(failures, [])
        self.assertEqual(report["scenario_count"], len(validator.REQUIRED_SCENARIOS))
        self.assertEqual(report["sandbox_account_fingerprint"], validator.fingerprint("acct_test_lifecycle"))
        self.assertNotIn("acct_test_lifecycle", Path(output_path).read_text(encoding="utf-8"))
        self.assertNotIn("id_personal_monthly_initial", Path(output_path).read_text(encoding="utf-8"))

    def test_skipped_case_fails_closed(self):
        value = evidence()
        value["scenarios"][0]["result"] = "not_run"
        report, failures = validator.validate(self.write(value))
        self.assertEqual(report["scenario_count"], len(validator.REQUIRED_SCENARIOS))
        self.assertEqual(failures, ["personal_monthly_initial: not_run"])

    def test_missing_case_is_rejected(self):
        value = evidence()
        value["scenarios"] = value["scenarios"][:-1]
        with self.assertRaisesRegex(ValueError, "exactly one row"):
            validator.validate(self.write(value))

    def test_wrong_api_version_is_rejected(self):
        value = evidence()
        value["api_version"] = "2026-08-26.dahlia"
        with self.assertRaisesRegex(ValueError, "api_version"):
            validator.validate(self.write(value))

    def test_provider_ids_are_not_emitted(self):
        value = evidence()
        value["scenarios"][0]["provider_ids"] = ["sub_secret_123"]
        report, _ = validator.validate(self.write(value))
        rendered = json.dumps(report)
        self.assertNotIn("sub_secret_123", rendered)


if __name__ == "__main__":
    unittest.main()
