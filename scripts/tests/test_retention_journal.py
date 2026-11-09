import importlib.machinery
import importlib.util
import json
import sys
import tempfile
import unittest
from unittest.mock import patch
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
LOADER = importlib.machinery.SourceFileLoader(
    "replay_retention_journal", str(ROOT / "scripts/replay-retention-journal")
)
SPEC = importlib.util.spec_from_loader(LOADER.name, LOADER)
replay = importlib.util.module_from_spec(SPEC)
sys.modules[LOADER.name] = replay
LOADER.exec_module(replay)


def entry(**overrides):
    value = {
        "format_version": 2,
        "journal_id": "journal-1",
        "job_id": "job-1",
        "resource_kind": "project",
        "resource_id": "project-1",
        "ownership_kind": "personal",
        "expected_owner_id": "user-1",
        "expected_created_at": 1_700_000_000_000_000,
        "expected_revision": None,
        "action": "deleted",
        "tombstone": {},
        "recorded_at": "2026-10-06T00:00:00Z",
    }
    value.update(overrides)
    return value


class Journal(unittest.TestCase):
    def write(self, values):
        handle = tempfile.NamedTemporaryFile(mode="w", encoding="utf-8", delete=False)
        with handle:
            for value in values:
                handle.write(json.dumps(value) + "\n")
        self.addCleanup(lambda: Path(handle.name).unlink(missing_ok=True))
        return handle.name

    def test_valid_entries_are_strictly_decoded(self):
        path = self.write([entry()])
        self.assertEqual(
            replay.parse_journal(path),
            [replay.Entry("journal-1", "project", "project-1", "personal", "user-1", 1_700_000_000_000_000, None)],
        )

    def test_duplicate_tombstone_identity_is_rejected(self):
        path = self.write([entry(), entry(journal_id="journal-2")])
        with self.assertRaisesRegex(ValueError, "repeats a tombstone identity"):
            replay.parse_journal(path)

    def test_reused_resource_id_with_a_new_creation_time_is_allowed(self):
        path = self.write(
            [
                entry(),
                entry(
                    journal_id="journal-2",
                    expected_created_at=1_700_000_000_000_001,
                ),
            ]
        )
        self.assertEqual(len(replay.parse_journal(path)), 2)

    def test_shared_entry_without_owner_is_rejected(self):
        path = self.write([entry(ownership_kind="shared", expected_owner_id=None)])
        with self.assertRaisesRegex(ValueError, "ownership kind"):
            replay.parse_journal(path)

    def test_unknown_format_versions_are_rejected(self):
        path = self.write([entry(format_version=3)])
        with self.assertRaisesRegex(ValueError, "unsupported format version"):
            replay.parse_journal(path)
        for invalid_version in (True, 1.0):
            with self.subTest(invalid_version=invalid_version):
                path = self.write([entry(format_version=invalid_version)])
                with self.assertRaisesRegex(ValueError, "unsupported format version"):
                    replay.parse_journal(path)

    def test_legacy_v1_personal_entries_remain_supported(self):
        path = self.write([entry(format_version=1)])
        self.assertEqual(len(replay.parse_journal(path)), 1)

    def test_v2_shared_grant_and_machine_token_entries_are_supported(self):
        path = self.write(
            [
                entry(
                    resource_kind="grant",
                    resource_id='["environment-1","user-1"]',
                    ownership_kind="shared",
                    expected_owner_id="organisation-1",
                ),
                entry(
                    journal_id="journal-2",
                    resource_kind="machine_token",
                    resource_id='["token-1","environment-1"]',
                    ownership_kind="shared",
                    expected_owner_id="organisation-1",
                ),
            ]
        )
        parsed = replay.parse_journal(path)
        self.assertEqual([row.resource_kind for row in parsed], ["grant", "machine_token"])
        self.assertEqual([row.ownership_kind for row in parsed], ["shared", "shared"])

    def test_shared_share_and_malformed_composite_identity_are_rejected(self):
        for invalid in (
            entry(resource_kind="share", ownership_kind="shared", expected_owner_id="organisation-1"),
            entry(resource_kind="grant", resource_id='["environment-1"]'),
        ):
            with self.subTest(invalid=invalid):
                path = self.write([invalid])
                with self.assertRaises(ValueError):
                    replay.parse_journal(path)

    def test_replay_deletes_child_rows_before_shared_parent_rows(self):
        entries = replay.parse_journal(
            self.write(
                [
                    entry(
                        resource_kind="grant",
                        resource_id='["environment-1","user-1"]',
                        ownership_kind="shared",
                        expected_owner_id="organisation-1",
                    ),
                    entry(
                        journal_id="journal-2",
                        resource_kind="machine_token",
                        resource_id='["token-1","environment-1"]',
                        ownership_kind="shared",
                        expected_owner_id="organisation-1",
                    ),
                    entry(
                        journal_id="journal-3",
                        resource_kind="project",
                        resource_id="project-1",
                        ownership_kind="shared",
                        expected_owner_id="organisation-1",
                    ),
                ]
            )
        )
        with patch.object(
            replay.subprocess,
            "run",
            return_value=type("Result", (), {"stdout": "DELETE 1\nDELETE 1\nDELETE 1\n3\n"})(),
        ) as run:
            self.assertEqual(replay.replay("postgres://localhost/restored", entries), (3, 3))
        sql = run.call_args.kwargs["input"]
        self.assertLess(sql.index("DELETE FROM environment_grants"), sql.index("DELETE FROM projects"))
        self.assertLess(sql.index("DELETE FROM machine_tokens"), sql.index("DELETE FROM projects"))
        self.assertIn("p.org_id = r.owner_id", sql)
        self.assertIn("g.created_at", sql)
        self.assertIn("m.created_at", sql)
        self.assertIn("e.revision = r.expected_revision", sql)
        self.assertIn("p.owner_id = r.owner_id", sql)
        self.assertIn("p.created_at", sql)

    def test_oversized_lines_are_rejected(self):
        path = self.write([entry(tombstone={"padding": "x" * replay.MAX_LINE_BYTES})])
        with self.assertRaisesRegex(ValueError, "exceeds"):
            replay.parse_journal(path)


if __name__ == "__main__":
    unittest.main()
