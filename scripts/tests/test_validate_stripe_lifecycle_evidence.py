import importlib.machinery
import importlib.util
import hashlib
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


class LifecycleEvidence(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        (self.root / "observations").mkdir()

    def artifact(self, scenario, *, app_sha="a" * 40, checks=None, provider_ids=None):
        check_records = checks if checks is not None else [
            {
                "id": check_id,
                "source": f"private://{scenario}/{check_id}",
                "observed": "observed in pinned test-mode run",
            }
            for check_id in validator.REQUIRED_CHECKS[scenario]
        ]
        value = {
            "schema": validator.OBSERVATION_SCHEMA,
            "scenario": scenario,
            "app_sha": app_sha,
            "api_version": validator.PINNED_API_VERSION,
            "captured_at": "2026-10-07T00:00:00Z",
            "wall_clock_at": "2026-10-07T00:00:00Z",
            "stripe_test_clock_epoch": 1791321600,
            "checks": check_records,
            "provider_ids": provider_ids if provider_ids is not None else [f"id_{scenario}"],
        }
        path = self.root / "observations" / f"{scenario}.json"
        raw = json.dumps(value, sort_keys=True, separators=(",", ":")).encode()
        path.write_bytes(raw)
        return f"observations/{scenario}.json", hashlib.sha256(raw).hexdigest()

    def evidence(self):
        scenarios = []
        for scenario in validator.REQUIRED_SCENARIOS:
            evidence_file, evidence_sha256 = self.artifact(scenario)
            scenarios.append(
                {
                    "scenario": scenario,
                    "result": "passed",
                    "evidence_file": evidence_file,
                    "evidence_sha256": evidence_sha256,
                }
            )
        return {
            "schema": validator.SCHEMA,
            "app_sha": "a" * 40,
            "api_version": validator.PINNED_API_VERSION,
            "sandbox_account_id": "acct_test_lifecycle",
            "captured_at": "2026-10-07T00:00:00Z",
            "image_digest": "sha256:" + "b" * 64,
            "migration_state": {"latest_applied": validator.latest_migration_version(), "pending": 0},
            "tool_versions": {key: "1.2.3" for key in validator.REQUIRED_TOOL_VERSIONS},
            "client_versions": {key: "1.2.3" for key in validator.REQUIRED_CLIENT_VERSIONS},
            "scenarios": scenarios,
        }

    def write(self, value):
        path = self.root / "manifest.json"
        path.write_text(json.dumps(value), encoding="utf-8")
        return path

    def output(self):
        return str(self.root / "sanitised.json")

    def rewrite_artifact(self, scenario, **changes):
        path = self.root / "observations" / f"{scenario}.json"
        value = json.loads(path.read_text(encoding="utf-8"))
        value.update(changes)
        raw = json.dumps(value, sort_keys=True, separators=(",", ":")).encode()
        path.write_bytes(raw)
        return hashlib.sha256(raw).hexdigest()

    def test_complete_run_hashes_private_observations_and_sanitises_report(self):
        manifest = self.write(self.evidence())
        report, failures = validator.validate(str(manifest))
        Path(self.output()).write_text(json.dumps(report), encoding="utf-8")
        self.assertEqual(failures, [])
        self.assertEqual(report["scenario_count"], len(validator.REQUIRED_SCENARIOS))
        self.assertEqual(report["manifest_sha256"], hashlib.sha256(manifest.read_bytes()).hexdigest())
        self.assertEqual(report["image_digest"], "sha256:" + "b" * 64)
        self.assertEqual(report["migration_state"]["pending"], 0)
        self.assertEqual(report["sandbox_account_fingerprint"], validator.fingerprint("acct_test_lifecycle"))
        interval = next(row for row in report["scenarios"] if row["scenario"] == "personal_interval_switch")
        self.assertIn("founding_interval_change_refused", interval["checks"])
        self.assertIn("founding_anniversary_keeps_original_interval", interval["checks"])
        self.assertEqual(
            interval["evidence_sha256"],
            hashlib.sha256((self.root / "observations/personal_interval_switch.json").read_bytes()).hexdigest(),
        )
        rendered = Path(self.output()).read_text(encoding="utf-8")
        self.assertNotIn("acct_test_lifecycle", rendered)
        self.assertNotIn("id_personal_monthly_initial", rendered)
        self.assertNotIn("observations/", rendered)
        self.assertNotIn("private://", rendered)

    def test_skipped_case_fails_closed_without_fabricated_observation(self):
        value = self.evidence()
        value["scenarios"][0].update({"result": "not_run", "evidence_file": "", "evidence_sha256": ""})
        report, failures = validator.validate(str(self.write(value)))
        self.assertEqual(report["scenario_count"], len(validator.REQUIRED_SCENARIOS))
        self.assertEqual(failures, [f"{value['scenarios'][0]['scenario']}: not_run"])

    def test_missing_case_is_rejected(self):
        value = self.evidence()
        value["scenarios"] = value["scenarios"][:-1]
        with self.assertRaisesRegex(ValueError, "exactly one row"):
            validator.validate(str(self.write(value)))

    def test_wrong_api_version_is_rejected(self):
        value = self.evidence()
        value["api_version"] = "2026-08-26.dahlia"
        with self.assertRaisesRegex(ValueError, "api_version"):
            validator.validate(str(self.write(value)))

    def test_candidate_identity_requires_full_lowercase_sha(self):
        value = self.evidence()
        value["app_sha"] = "abcdef0"
        with self.assertRaisesRegex(ValueError, "full lowercase commit SHA"):
            validator.validate(str(self.write(value)))

    def test_stale_migration_version_is_rejected(self):
        value = self.evidence()
        value["migration_state"]["latest_applied"] = "0001"
        with self.assertRaisesRegex(ValueError, "match current version"):
            validator.validate(str(self.write(value)))

    def test_duplicate_manifest_json_keys_are_rejected(self):
        value = self.evidence()
        rendered = json.dumps(value)
        rendered = rendered.replace(
            '"app_sha": "' + value["app_sha"] + '"',
            '"app_sha": "' + value["app_sha"] + '", "app_sha": "' + value["app_sha"] + '"',
            1,
        )
        path = self.root / "duplicate-manifest.json"
        path.write_text(rendered, encoding="utf-8")
        with self.assertRaisesRegex(validator.DuplicateJsonKeyError, "duplicate JSON key: app_sha"):
            validator.validate(str(path))

    def test_duplicate_observation_json_keys_are_rejected(self):
        value = self.evidence()
        row = value["scenarios"][0]
        observation_path = self.root / row["evidence_file"]
        raw = observation_path.read_text(encoding="utf-8")
        row_app_sha = '"app_sha":"' + value["app_sha"] + '"'
        duplicate = row_app_sha + ',"app_sha":"' + value["app_sha"] + '"'
        observation_path.write_text(raw.replace(row_app_sha, duplicate, 1), encoding="utf-8")
        row["evidence_sha256"] = hashlib.sha256(observation_path.read_bytes()).hexdigest()
        with self.assertRaisesRegex(ValueError, "observation .* is not valid JSON"):
            validator.validate(str(self.write(value)))

    def test_template_contains_every_case_and_cannot_pass_as_is(self):
        destination = self.output()
        self.assertEqual(validator.main(["--template", destination]), 0)
        value = json.loads(Path(destination).read_text(encoding="utf-8"))
        self.assertEqual(
            [row["scenario"] for row in value["scenarios"]],
            list(validator.REQUIRED_SCENARIOS),
        )
        self.assertTrue(all(not row["evidence_file"] and not row["evidence_sha256"] for row in value["scenarios"]))
        value["app_sha"] = "a" * 40
        for row in value["scenarios"]:
            row["result"] = "passed"
        with self.assertRaisesRegex(ValueError, "requires a private observation file"):
            validator.validate(str(self.write(value)))

    def test_every_required_scenario_has_a_nonempty_unique_check_contract(self):
        self.assertEqual(set(validator.REQUIRED_CHECKS), set(validator.REQUIRED_SCENARIOS))
        for scenario, checks in validator.REQUIRED_CHECKS.items():
            self.assertTrue(checks, scenario)
            self.assertEqual(len(checks), len(set(checks)), scenario)

    def test_template_and_validation_modes_cannot_be_combined(self):
        with self.assertRaises(SystemExit) as error:
            validator.main(["evidence.json", "--template", self.output(), "--output", self.output()])
        self.assertEqual(error.exception.code, 2)

    def test_provider_ids_are_not_emitted(self):
        value = self.evidence()
        scenario = value["scenarios"][0]["scenario"]
        digest = self.rewrite_artifact(scenario, provider_ids=["sub_secret_123"])
        value["scenarios"][0]["evidence_sha256"] = digest
        report, _ = validator.validate(str(self.write(value)))
        self.assertNotIn("sub_secret_123", json.dumps(report))

    def test_free_form_claim_cannot_replace_required_observation_checks(self):
        value = self.evidence()
        scenario = "personal_interval_switch"
        digest = self.rewrite_artifact(
            scenario,
            checks=[{"id": "interval was safely handled", "source": "private://source", "observed": "yes"}],
        )
        row = next(row for row in value["scenarios"] if row["scenario"] == scenario)
        row["evidence_sha256"] = digest
        with self.assertRaisesRegex(ValueError, "exact required checks"):
            validator.validate(str(self.write(value)))

    def test_interval_change_scenario_requires_refusal_without_billing_mutation(self):
        value = self.evidence()
        scenario = "personal_interval_switch"
        checks = [
            {
                "id": "founding_interval_change_applied" if check == "founding_interval_change_refused" else check,
                "source": f"private://{scenario}/{check}",
                "observed": "observed in pinned test-mode run",
            }
            for check in validator.REQUIRED_CHECKS[scenario]
        ]
        digest = self.rewrite_artifact(scenario, checks=checks)
        row = next(row for row in value["scenarios"] if row["scenario"] == scenario)
        row["evidence_sha256"] = digest
        with self.assertRaisesRegex(ValueError, "exact required checks"):
            validator.validate(str(self.write(value)))

    def test_schema_v4_is_required_for_structured_observations(self):
        value = self.evidence()
        value["schema"] = "sotto-stripe-lifecycle-acceptance-v3"
        with self.assertRaisesRegex(ValueError, "unsupported lifecycle evidence schema"):
            validator.validate(str(self.write(value)))

    def test_image_migration_tool_and_client_provenance_is_required(self):
        value = self.evidence()
        value["image_digest"] = "mutable:latest"
        with self.assertRaisesRegex(ValueError, "image_digest"):
            validator.validate(str(self.write(value)))

        value = self.evidence()
        value["migration_state"]["pending"] = 1
        with self.assertRaisesRegex(ValueError, "pending must be zero"):
            validator.validate(str(self.write(value)))

        value = self.evidence()
        del value["client_versions"]["previous_minor_cli"]
        with self.assertRaisesRegex(ValueError, "client_versions must contain exactly"):
            validator.validate(str(self.write(value)))

    def test_clock_evidence_keeps_wall_and_stripe_test_clock_separate(self):
        value = self.evidence()
        scenario = value["scenarios"][0]["scenario"]
        digest = self.rewrite_artifact(scenario, stripe_test_clock_epoch="1791321600")
        value["scenarios"][0]["evidence_sha256"] = digest
        with self.assertRaisesRegex(ValueError, "positive Unix timestamp or null"):
            validator.validate(str(self.write(value)))

    def test_non_string_result_is_a_validation_error(self):
        value = self.evidence()
        value["scenarios"][0]["result"] = ["passed"]
        with self.assertRaisesRegex(ValueError, "invalid result"):
            validator.validate(str(self.write(value)))

    def test_observation_digest_must_match_file_bytes(self):
        value = self.evidence()
        scenario = value["scenarios"][0]["scenario"]
        path = self.root / "observations" / f"{scenario}.json"
        path.write_text(path.read_text(encoding="utf-8") + " ", encoding="utf-8")
        with self.assertRaisesRegex(ValueError, "does not match its observation file"):
            validator.validate(str(self.write(value)))

    def test_observation_must_be_bound_to_candidate_sha(self):
        value = self.evidence()
        scenario = value["scenarios"][0]["scenario"]
        digest = self.rewrite_artifact(scenario, app_sha="b" * 40)
        value["scenarios"][0]["evidence_sha256"] = digest
        with self.assertRaisesRegex(ValueError, "different app SHA"):
            validator.validate(str(self.write(value)))


if __name__ == "__main__":
    unittest.main()
