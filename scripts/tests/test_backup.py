"""Behaviour of deploy/backup.sh, against a stubbed host.

The script runs unattended from cron into a log that is read only after the freshness alarm has
fired. These tests pin down the two things that log has to answer at that point: did the run
succeed, and if not, which night and at which step. They also pin the exit status, because a run
that uploaded nothing must never print `backup ok`. An earlier version did exactly that when the
dump upload failed, since `upload && upload` under `set -e` swallows a failure on the left.
"""

import os
import re
import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
SCRIPT = ROOT / "deploy/backup.sh"

# pg_dump emits an archive, pg_restore --list rejects an empty one, psql emits an empty journal.
DOCKER_STUB = """#!/bin/sh
case " $* " in
  *" pg_dump "*) [ -n "$STUB_DUMP_FAILS" ] && { echo "pg_dump: connection refused" >&2; exit 1; }
                 printf 'PGDMP-stub-archive' ;;
  *" pg_restore "*) [ -s /dev/stdin ] || { echo "pg_restore: input file is too short" >&2; exit 1; }
                    cat > /dev/null ;;
  *" psql "*) : ;;
esac
"""

# Records every upload, and fails the one whose destination matches STUB_UPLOAD_FAILS.
UPLOAD_STUB = """#!/bin/sh
for last; do :; done
case "$last" in *"${STUB_UPLOAD_FAILS:-<none>}"*) echo "upload refused: $last" >&2; exit 1 ;; esac
echo "$last" >> "$STUB_UPLOADS"
"""

TIMESTAMP = re.compile(r"^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}Z ")
OWN_LINE = re.compile(r"backup (started|ok|FAILED)|retention journal ok|error: ")


class Backup(unittest.TestCase):
    def run_script(self, bucket="gs://sotto-backups-test", tools=("docker", "gsutil"), **environ):
        work = Path(self.enterContext(tempfile.TemporaryDirectory()))
        bin_dir = work / "bin"
        bin_dir.mkdir()
        for name in tools:
            (bin_dir / name).write_text(DOCKER_STUB if name == "docker" else UPLOAD_STUB)
            (bin_dir / name).chmod(0o755)
        # Only what the script needs from the system, so a tool left out is genuinely absent.
        for name in ("sh", "dirname", "sed", "date", "mktemp", "rm", "mkdir", "wc", "tr", "cat",
                     "python3"):
            found = shutil.which(name)
            if found:
                (bin_dir / name).symlink_to(found)
        uploads = work / "uploads"
        uploads.touch()
        env = {
            "PATH": str(bin_dir),
            "HOME": str(work),
            "SOTTO_BACKUP_BUCKET": bucket,
            "STUB_UPLOADS": str(uploads),
            **environ,
        }
        result = subprocess.run(
            ["/bin/sh", str(SCRIPT)], env=env, capture_output=True, text=True, timeout=60
        )
        return result, uploads.read_text().splitlines()

    def assert_timestamped(self, output):
        # Tool errors pass through verbatim; the script's own lines are the ones that must be dated.
        ours = [line for line in output.splitlines() if OWN_LINE.search(line)]
        self.assertTrue(ours)
        for line in ours:
            self.assertRegex(line, TIMESTAMP)

    def test_success_uploads_both_objects_and_says_so(self):
        result, uploads = self.run_script()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(len(uploads), 2)
        self.assertTrue(uploads[0].endswith(".dump"))
        self.assertTrue(uploads[1].endswith(".tombstones.jsonl"))
        self.assertIn("backup started", result.stdout)
        self.assertIn("backup ok: gs://sotto-backups-test/sotto-", result.stdout)
        self.assertNotIn("FAILED", result.stdout + result.stderr)
        self.assert_timestamped(result.stdout)

    def test_failed_dump_upload_is_a_failure_not_ok(self):
        result, uploads = self.run_script(STUB_UPLOAD_FAILS=".dump")
        self.assertNotEqual(result.returncode, 0)
        self.assertNotIn("backup ok", result.stdout)
        self.assertIn("backup FAILED while uploading the dump", result.stderr)
        self.assertEqual(uploads, [])
        self.assert_timestamped(result.stderr)

    def test_failed_journal_upload_is_a_failure(self):
        result, uploads = self.run_script(STUB_UPLOAD_FAILS=".tombstones.jsonl")
        self.assertNotEqual(result.returncode, 0)
        self.assertNotIn("backup ok", result.stdout)
        self.assertIn("backup FAILED while uploading the retention journal", result.stderr)

    def test_failed_dump_names_the_step_and_uploads_nothing(self):
        result, uploads = self.run_script(STUB_DUMP_FAILS="1")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("backup FAILED while dumping the database", result.stderr)
        self.assertEqual(uploads, [])

    def test_missing_upload_tool_fails_before_the_dump_and_names_cron_path(self):
        result, uploads = self.run_script(tools=("docker",))
        self.assertEqual(result.returncode, 127)
        self.assertIn("gsutil is not on PATH", result.stderr)
        self.assertIn("crontab", result.stderr)
        self.assertIn("backup FAILED while checking tools", result.stderr)
        self.assertEqual(uploads, [])

    def test_uploader_follows_the_destination_scheme(self):
        result, uploads = self.run_script(bucket="s3://sotto-backups-test", tools=("docker", "aws"))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(len(uploads), 2)
        self.assertTrue(all(u.startswith("s3://sotto-backups-test/") for u in uploads))

    def test_unset_destination_is_reported(self):
        # No deploy/.env exists in the checkout, which is also the case this covers: a missing file
        # must reach the explanation, not end the run silently on sed's exit status.
        result, _ = self.run_script(bucket="")
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertIn("SOTTO_BACKUP_BUCKET is not set", result.stderr)
        self.assertIn("backup FAILED while reading configuration", result.stderr)

    def test_no_dump_is_left_behind(self):
        before = set(Path("/tmp").glob("sotto-pgdump.*"))
        for environ in ({}, {"STUB_UPLOAD_FAILS": ".dump"}, {"STUB_DUMP_FAILS": "1"}):
            self.run_script(**environ)
        self.assertEqual(set(Path("/tmp").glob("sotto-pgdump.*")), before)


if __name__ == "__main__":
    unittest.main()
