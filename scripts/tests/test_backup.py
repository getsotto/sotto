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

# pg_dump emits an archive, pg_restore --list rejects an empty one. psql answers the
# table-existence probe and otherwise emits an empty journal; with STUB_NO_JOURNAL_TABLE it
# behaves like a pre-migration-50 database, where the journal table is absent and any query
# against it fails.
DOCKER_STUB = """#!/bin/sh
case " $* " in
  *" compose version "*) if [ -n "${STUB_NO_COMPOSE:-}" ]; then echo "docker: 'compose' is not a docker command" >&2; exit 1; fi ;;
  *" pg_dump "*) if [ -n "${STUB_DUMP_FAILS:-}" ]; then echo "pg_dump: connection refused" >&2; exit 1; fi
                 printf 'PGDMP-stub-archive' ;;
  *" pg_restore "*) if [ -s /dev/stdin ]; then cat > /dev/null; else echo "pg_restore: input file is too short" >&2; exit 1; fi ;;
  *"to_regclass"*) if [ -z "${STUB_NO_JOURNAL_TABLE:-}" ]; then echo "cloud_retention_tombstone_journal"; fi ;;
  *" psql "*) if [ -n "${STUB_NO_JOURNAL_TABLE:-}" ]; then echo 'psql: error: relation "cloud_retention_tombstone_journal" does not exist' >&2; exit 1; fi ;;
esac
"""

# Records every upload, fails the one whose destination matches STUB_UPLOAD_FAILS, and with
# STUB_FLAKY_FIRST_UPLOAD fails the first upload call only, like a transient network blip.
UPLOAD_STUB = """#!/bin/sh
for last; do :; done
case "$last" in *"${STUB_UPLOAD_FAILS:-<none>}"*) echo "upload refused: $last" >&2; exit 1 ;; esac
if [ -n "${STUB_FLAKY_FIRST_UPLOAD:-}" ] && [ ! -e "$STUB_UPLOADS.flaky" ]; then
  : > "$STUB_UPLOADS.flaky"
  echo "upload transient failure: $last" >&2
  exit 1
fi
echo "$last" >> "$STUB_UPLOADS"
"""

TIMESTAMP = re.compile(r"^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}Z ")
OWN_LINE = re.compile(r"backup (started|ok|FAILED)|retention journal ok|error: ")


class Backup(unittest.TestCase):
    def run_script(self, bucket="gs://sotto-backups-test", tools=("docker", "gsutil"), env_file=None,
                   env_mode=0o600, extra_system=(), **environ):
        work = Path(self.enterContext(tempfile.TemporaryDirectory()))
        script = SCRIPT
        if env_file is not None:
            # A private copy of the tree, so the test controls deploy/.env rather than inheriting
            # whatever the developer's checkout holds there. scripts/ is shared, read-only.
            (work / "deploy").mkdir()
            script = work / "deploy/backup.sh"
            shutil.copy2(SCRIPT, script)
            (work / "scripts").symlink_to(ROOT / "scripts")
            (work / "deploy/.env").write_text(env_file)
            (work / "deploy/.env").chmod(env_mode)
        bin_dir = work / "bin"
        bin_dir.mkdir()
        for name in tools:
            (bin_dir / name).write_text(DOCKER_STUB if name == "docker" else UPLOAD_STUB)
            (bin_dir / name).chmod(0o755)
        # Only what the script needs from the system, so a tool left out is genuinely absent.
        for name in ("sh", "dirname", "sed", "date", "mktemp", "rm", "mkdir", "wc", "tr", "cat",
                     "python3", "sleep", "id", *extra_system):
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
            ["/bin/sh", str(script)], env=env, capture_output=True, text=True, timeout=60
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

    def test_transient_upload_failure_is_retried_not_failed(self):
        # One missed night breaches the freshness threshold, so a single blip must not fail
        # the run. Before the retry, this stubbed blip ended the run with `backup FAILED`.
        result, uploads = self.run_script(STUB_FLAKY_FIRST_UPLOAD="1")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(len(uploads), 2)
        self.assertIn("backup ok: gs://sotto-backups-test/sotto-", result.stdout)
        self.assertIn("retrying", result.stderr)

    def test_pre_retention_database_still_backs_up(self):
        # A database without the journal table (pre-migration-50) exports an empty journal
        # instead of failing the run, mirroring check-restore which accepts an empty journal
        # for those backups. Before that, the export query failed and no dump was uploaded.
        result, uploads = self.run_script(STUB_NO_JOURNAL_TABLE="1")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(len(uploads), 2)
        self.assertIn("backup ok: gs://sotto-backups-test/sotto-", result.stdout)
        self.assertIn("pre-migration-50", result.stderr)

    def test_missing_compose_plugin_fails_before_the_dump_and_names_it(self):
        # The docker binary being present does not mean `docker compose` is installed.
        result, uploads = self.run_script(STUB_NO_COMPOSE="1")
        self.assertEqual(result.returncode, 127)
        self.assertIn("docker compose", result.stderr)
        self.assertIn("backup FAILED while checking tools", result.stderr)
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

    @unittest.skipIf(os.geteuid() == 0, "root reads a mode-000 file, so nothing is unreadable")
    def test_unreadable_env_is_named_not_reported_as_unset(self):
        # A sudo edit can leave deploy/.env as root:root 600, and the cron user then cannot read
        # it. Reporting "not set" would send the operator to add a setting that
        # is already there. The bucket comes from .env here, as it does on the host.
        result, uploads = self.run_script(
            bucket="", env_file="SOTTO_BACKUP_BUCKET=gs://sotto-backups-test\n", env_mode=0o000
        )
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertIn("deploy/.env exists but", result.stderr)
        self.assertIn("cannot read it", result.stderr)
        self.assertIn("sudo chown", result.stderr)
        self.assertNotIn("is not set", result.stderr)
        self.assertIn("backup FAILED while reading configuration", result.stderr)
        self.assertEqual(uploads, [])
        self.assert_timestamped(result.stderr)

    def test_readable_env_supplies_the_destination(self):
        # The control for the test above: the same private tree, readable, backs up normally, so
        # the failure there is the permission and not the copied layout.
        result, uploads = self.run_script(
            bucket="", env_file="SOTTO_BACKUP_BUCKET=gs://sotto-backups-test\n"
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(len(uploads), 2)
        self.assertTrue(all(u.startswith("gs://sotto-backups-test/") for u in uploads))

    def test_start_line_names_the_script_version(self):
        # The stubbed PATH has no git, as on a deployment that is not a checkout: that must log
        # `unknown` rather than fail the run.
        result, uploads = self.run_script()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("backup started (script unknown)", result.stdout)
        if shutil.which("git") is None:
            self.skipTest("no git to read the checkout with")
        result, _ = self.run_script(extra_system=("git",))
        head = subprocess.run(["git", "-C", str(ROOT), "rev-parse", "--short", "HEAD"],
                              capture_output=True, text=True, check=True).stdout.strip()
        self.assertIn(f"backup started (script {head})", result.stdout)

    def test_no_dump_is_left_behind(self):
        before = set(Path("/tmp").glob("sotto-pgdump.*"))
        for environ in ({}, {"STUB_UPLOAD_FAILS": ".dump"}, {"STUB_DUMP_FAILS": "1"}):
            self.run_script(**environ)
        self.assertEqual(set(Path("/tmp").glob("sotto-pgdump.*")), before)


if __name__ == "__main__":
    unittest.main()
