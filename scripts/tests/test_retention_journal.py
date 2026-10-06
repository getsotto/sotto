import importlib.machinery
import importlib.util
import json
import sys
import tempfile
import unittest
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
        "format_version": 1,
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
            [replay.Entry("journal-1", "project", "project-1", "user-1", 1_700_000_000_000_000, None)],
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

    def test_shared_and_unknown_entries_are_rejected(self):
        path = self.write([entry(ownership_kind="shared", expected_owner_id=None)])
        with self.assertRaisesRegex(ValueError, "not a personal tombstone"):
            replay.parse_journal(path)

    def test_unknown_format_versions_are_rejected(self):
        path = self.write([entry(format_version=2)])
        with self.assertRaisesRegex(ValueError, "unsupported format version"):
            replay.parse_journal(path)

    def test_oversized_lines_are_rejected(self):
        path = self.write([entry(tombstone={"padding": "x" * replay.MAX_LINE_BYTES})])
        with self.assertRaisesRegex(ValueError, "exceeds"):
            replay.parse_journal(path)


if __name__ == "__main__":
    unittest.main()
