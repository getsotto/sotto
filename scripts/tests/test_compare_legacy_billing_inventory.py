import importlib.machinery
import importlib.util
import json
import sys
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
LOADER = importlib.machinery.SourceFileLoader(
    "compare_legacy_billing_inventory", str(ROOT / "scripts/compare-legacy-billing-inventory")
)
SPEC = importlib.util.spec_from_loader(LOADER.name, LOADER)
compare = importlib.util.module_from_spec(SPEC)
sys.modules[LOADER.name] = compare
LOADER.exec_module(compare)


def report(timestamp="2026-10-07T00:00:00Z"):
    return {
        "inventory_version": 1,
        "generated_at": timestamp,
        "source": "read_only_postgres_aggregate",
        "provenance": {"latest_migration": "50", "postgres_version_num": "160000"},
        "cohorts": [{"category": "organisation_cohort", "label": "legacy_free", "count": 1, "proposed_treatment": "preserve_free_access"}],
        "activation": {"writes_performed": False, "charges_created": False, "automatic_transition_approved": False, "decision_gate": "D09"},
    }


class Comparison(unittest.TestCase):
    def write(self, value):
        handle = tempfile.NamedTemporaryFile(mode="w", encoding="utf-8", delete=False)
        with handle:
            json.dump(value, handle)
        self.addCleanup(lambda: Path(handle.name).unlink(missing_ok=True))
        return handle.name

    def test_generated_time_is_ignored(self):
        first = self.write(report())
        second = self.write(report("2026-10-07T01:00:00Z"))
        self.assertEqual(compare.comparable(compare.load(first)), compare.comparable(compare.load(second)))

    def test_cohort_change_is_not_ignored(self):
        first = self.write(report())
        changed = report()
        changed["cohorts"][0]["count"] = 2
        second = self.write(changed)
        self.assertNotEqual(compare.comparable(compare.load(first)), compare.comparable(compare.load(second)))

    def test_activation_flags_must_remain_disabled(self):
        bad = report()
        bad["activation"]["writes_performed"] = True
        with self.assertRaisesRegex(ValueError, "not a read-only"):
            compare.load(self.write(bad))

    def test_incomplete_report_is_rejected(self):
        with self.assertRaisesRegex(ValueError, "incomplete inventory shape"):
            compare.load(self.write({"inventory_version": 1, "activation": {"writes_performed": False}}))

    def test_incomplete_cohort_row_is_rejected(self):
        bad = report()
        del bad["cohorts"][0]["count"]
        with self.assertRaisesRegex(ValueError, "invalid cohort row"):
            compare.load(self.write(bad))


if __name__ == "__main__":
    unittest.main()
