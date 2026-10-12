import importlib.machinery
import importlib.util
import json
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

ROOT = Path(__file__).resolve().parents[2]
LOADER = importlib.machinery.SourceFileLoader(
    "inventory_legacy_billing", str(ROOT / "scripts/inventory-legacy-billing")
)
SPEC = importlib.util.spec_from_loader(LOADER.name, LOADER)
inventory = importlib.util.module_from_spec(SPEC)
sys.modules[LOADER.name] = inventory
LOADER.exec_module(inventory)


class Treatment(unittest.TestCase):
    def test_paid_is_preserved_until_the_cohort_is_approved(self):
        self.assertEqual(
            inventory.proposed_treatment("legacy_paid"),
            "preserve_legacy_coverage_pending_approval",
        )

    def test_ambiguous_rows_are_quarantined(self):
        for label in (
            "legacy_manual_team",
            "quarantine_provider_link_incomplete",
            "quarantine_paid_tier_mismatch",
        ):
            self.assertIn("quarantine", inventory.proposed_treatment(label))

    def test_unknown_categories_are_report_only(self):
        self.assertEqual(inventory.proposed_treatment("personal_active"), "report_only")


class Report(unittest.TestCase):
    def test_report_has_no_activation_side_effects(self):
        report = inventory.build_report(
            [("organisation_cohort", "legacy_paid", 2), ("share_link_state", "active", 4)],
            {"latest_migration": "50", "postgres_version_num": "160000"},
        )
        self.assertEqual(report["inventory_version"], 1)
        self.assertFalse(report["activation"]["writes_performed"])
        self.assertFalse(report["activation"]["charges_created"])
        self.assertEqual(report["cohorts"][0]["count"], 2)

    def test_output_replacement_never_leaves_partial_report(self):
        report = inventory.build_report([], {"latest_migration": "50", "postgres_version_num": "160000"})
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "inventory.json"
            inventory.write_report(report, str(path))
            loaded = json.loads(path.read_text())
            self.assertEqual(loaded["source"], "read_only_postgres_aggregate")
            self.assertEqual(path.stat().st_mode & 0o777, 0o600)


class Database(unittest.TestCase):
    def test_inventory_query_is_read_only(self):
        self.assertNotRegex(inventory.INVENTORY_SQL, r"(?i)\b(insert|update|delete|alter|drop)\b")

    def test_deleted_organisation_tombstones_are_not_live_cohorts(self):
        self.assertEqual(inventory.INVENTORY_SQL.count("WHERE lifecycle_state <> 'deleted'"), 2)

    def test_deleted_organisation_sponsorships_are_quarantined(self):
        self.assertIn("quarantine_deleted_organisation", inventory.INVENTORY_SQL)
        self.assertIn("JOIN organizations o ON o.id = s.organization_id", inventory.INVENTORY_SQL)

    def test_database_password_is_not_an_argument(self):
        completed = mock.Mock(stdout="organisation_cohort\tlegacy_free\t1\n")
        provenance = mock.Mock(stdout="50|160000\n")
        with mock.patch.object(inventory.subprocess, "run", side_effect=[completed, provenance]) as run:
            rows, source = inventory.read_database("postgres://sotto:hunter2@db.example:5433/sotto")
        self.assertEqual(rows, [("organisation_cohort", "legacy_free", 1)])
        self.assertEqual(source["latest_migration"], "50")
        for call in run.call_args_list:
            self.assertNotIn("hunter2", " ".join(call.args[0]))
            self.assertEqual(call.kwargs["env"]["PGPASSWORD"], "hunter2")

    def test_database_url_can_come_from_the_environment(self):
        with mock.patch.dict(inventory.os.environ, {"INVENTORY_DATABASE_URL": "postgres://db/sotto"}):
            with mock.patch.object(inventory, "read_database", return_value=([], {"latest_migration": "50", "postgres_version_num": "160000"})) as read:
                self.assertEqual(inventory.main([]), 0)
        read.assert_called_once_with("postgres://db/sotto")

    def test_database_url_preserves_libpq_tls_options(self):
        env = inventory.connection_env(
            "postgres://u:p@db/sotto?sslmode=verify-full&sslrootcert=%2Fetc%2Fca.pem"
        )
        self.assertEqual(env["PGSSLMODE"], "verify-full")
        self.assertEqual(env["PGSSLROOTCERT"], "/etc/ca.pem")

    def test_repeated_tls_options_are_rejected(self):
        with self.assertRaisesRegex(ValueError, "repeated sslmode"):
            inventory.connection_env("postgres://db/sotto?sslmode=require&sslmode=verify-full")


if __name__ == "__main__":
    unittest.main()
