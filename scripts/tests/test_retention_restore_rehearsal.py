import hashlib
import hmac
import importlib.machinery
import importlib.util
import json
import os
import tempfile
import unittest
from argparse import Namespace
from pathlib import Path
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[2]
LOADER = importlib.machinery.SourceFileLoader(
    "retention_restore_rehearsal", str(ROOT / "scripts/retention-restore-rehearsal")
)
SPEC = importlib.util.spec_from_loader(LOADER.name, LOADER)
rehearsal = importlib.util.module_from_spec(SPEC)
import sys
from types import SimpleNamespace

sys.modules[LOADER.name] = rehearsal
LOADER.exec_module(rehearsal)


class RetentionRestoreRehearsal(unittest.TestCase):
    def test_only_dedicated_local_scratch_database_is_allowed(self):
        rehearsal.local_scratch_database("postgres://localhost/retention_restore_ci")
        for url in (
            "postgres://example.test/retention_restore_ci",
            "postgres://localhost/sotto",
        ):
            with self.subTest(url=url), self.assertRaises(ValueError):
                rehearsal.local_scratch_database(url)

    def test_signature_matches_server_length_prefixed_payload(self):
        evidence = {
            "evidence_id": "restore-evidence:test",
            "release_sha": "a" * 40,
            "backup_fingerprint": "b" * 64,
            "sidecar_sha256": "c" * 64,
            "journal_entry_count": 4,
            "replay_entry_count": 4,
            "first_replay_deleted_count": 3,
            "second_replay_deleted_count": 0,
            "incarnation_guard_verified": True,
        }
        payload = (
            "32:sotto-cloud-retention-restore-v1\n"
            "21:restore-evidence:test\n"
            f"40:{'a' * 40}\n"
            f"64:{'b' * 64}\n"
            f"64:{'c' * 64}\n"
            "1:4\n1:4\n1:3\n1:0\n4:true"
        ).encode("ascii")
        self.assertEqual(rehearsal.evidence_payload(evidence), payload)
        signed = rehearsal.sign_evidence(evidence, b"retention evidence signing key long enough")
        self.assertEqual(
            signed["signature"],
            hmac.new(b"retention evidence signing key long enough", payload, hashlib.sha256).hexdigest(),
        )

    def test_rehearsal_emits_private_signed_request_after_two_replays(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            backup = root / "backup.dump"
            journal = root / "journal.jsonl"
            key_path = root / "evidence.key"
            for path, content in ((backup, b"backup"), (journal, b"journal"), (key_path, b"k" * 32)):
                path.write_bytes(content)
            key_path.chmod(0o600)
            output = root / "evidence.json"
            args = Namespace(
                backup=str(backup),
                journal=str(journal),
                evidence_key_file=str(key_path),
                release_sha="a" * 40,
                operator="reviewer",
                output=str(output),
            )
            with (
                patch.dict(os.environ, {"SOTTO_RETENTION_REHEARSAL_DATABASE_URL": "postgres://localhost/retention_restore_ci"}),
                patch.object(
                    rehearsal.subprocess,
                    "run",
                    side_effect=[
                        SimpleNamespace(stdout="a" * 40),
                        SimpleNamespace(stdout=""),
                        SimpleNamespace(stdout=""),
                    ],
                ) as run_process,
                patch.object(rehearsal.replay, "parse_journal", return_value=[object()]),
                patch.object(rehearsal.replay, "replay", side_effect=[(1, 1), (1, 0)]) as replay_twice,
                patch.object(rehearsal.restore_check, "repository_migrations", return_value=["1"]),
                patch.object(rehearsal, "validate_restored_database") as check_database,
                patch.object(rehearsal, "verify_incarnation_guards") as check_guards,
            ):
                result = rehearsal.run(args)

            self.assertEqual(replay_twice.call_count, 2)
            self.assertEqual(check_database.call_count, 2)
            check_guards.assert_called_once()
            self.assertEqual(result["operator"], "reviewer")
            self.assertEqual(result["evidence"]["first_replay_deleted_count"], 1)
            self.assertTrue(result["evidence"]["incarnation_guard_verified"])
            rehearsal.write_private_json(output, result)
            self.assertEqual(output.stat().st_mode & 0o777, 0o600)
            self.assertEqual(json.loads(output.read_text()), result)


if __name__ == "__main__":
    unittest.main()
