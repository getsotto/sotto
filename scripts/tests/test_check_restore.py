"""Restore assertions: what a dump has to prove before anyone would trust it.

The rehearsal these encode was run by hand on 2026-08-31 against the first backup taken from
the post-upgrade schema. The point of writing it down is that a drill nobody repeats is a drill
that stops being true, which is the same failure the backup cron itself had.
"""

import importlib.machinery
import importlib.util
import subprocess
import sys
import tempfile
import unittest
import unittest.mock
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
LOADER = importlib.machinery.SourceFileLoader("check_restore", str(ROOT / "scripts/check-restore"))
SPEC = importlib.util.spec_from_loader(LOADER.name, LOADER)
restore = importlib.util.module_from_spec(SPEC)
sys.modules[LOADER.name] = restore
LOADER.exec_module(restore)

REPO = [str(n) for n in range(1, 22)]


class Migrations(unittest.TestCase):
    def test_a_healthy_dump_passes(self):
        applied = [(v, "t") for v in REPO]
        self.assertEqual(restore.check_migrations(applied, REPO), [])

    def test_a_deployment_behind_this_branch_is_not_a_failure(self):
        # The property that decides whether anyone keeps paying attention to this job. A
        # migration merges, production has not taken it yet, and every monthly run in between
        # would go red for a backup that is completely fine.
        applied = [(v, "t") for v in REPO[:-3]]
        self.assertEqual(restore.check_migrations(applied, REPO), [])

    def test_a_deployment_ahead_of_this_branch_is_a_failure(self):
        # The other direction is not symmetrical. A dump carrying schema this checkout does not
        # know about cannot be restored into something this code can run, which is the only
        # question a restore drill is asking.
        applied = [(v, "t") for v in REPO + ["22"]]
        problems = restore.check_migrations(applied, REPO)
        self.assertEqual(len(problems), 1)
        self.assertIn("22", problems[0])
        self.assertIn("ahead of this branch", problems[0])

    def test_a_migration_recorded_as_failed_is_caught(self):
        applied = [(v, "t") for v in REPO[:-1]] + [("21", "f")]
        problems = restore.check_migrations(applied, REPO)
        self.assertIn("recorded as failed: 21", problems[0])

    def test_a_dump_with_no_migrations_at_all_is_caught(self):
        # What restoring an empty or wrong database looks like from here.
        problems = restore.check_migrations([], REPO)
        self.assertIn("records no migrations", problems[0])


class Tables(unittest.TestCase):
    def test_the_deletion_tables_are_required(self):
        # Named because organisation deletion is the one thing in Sotto that cannot be undone
        # from inside the product, so this backup is the only thing behind it.
        present = ["users", "organizations", "audit_events"]
        problems = restore.check_tables(present, [("21", "t")])
        self.assertIn("organization_deletions", problems[0])

    def test_a_complete_schema_passes(self):
        self.assertEqual(restore.check_tables(list(restore.MUST_EXIST), [("22", "t")]), [])

    def test_pre_retention_backup_does_not_require_a_future_table(self):
        present = list(restore.MUST_EXIST)
        self.assertEqual(restore.check_tables(present, [("22", "t")]), [])

    def test_retention_backup_requires_its_journal_table(self):
        present = list(restore.MUST_EXIST)
        problems = restore.check_tables(present, [("50", "t")])
        self.assertEqual(len(problems), 1)
        self.assertIn("cloud_retention_tombstone_journal", problems[0])

    def test_retention_backup_with_journal_passes(self):
        present = list(restore.MUST_EXIST) + ["cloud_retention_tombstone_journal"]
        self.assertEqual(restore.check_tables(present, [("50", "t")]), [])


class Emptiness(unittest.TestCase):
    def test_a_restore_that_carried_no_rows_is_caught(self):
        # A dump of the wrong database, or of one truncated before the dump ran, passes every
        # structural check ever written. This is the one that notices.
        problems = restore.check_not_empty({"users": 0, "organizations": 1})
        self.assertIn("restored but empty: users", problems[0])

    def test_rows_present_passes(self):
        self.assertEqual(restore.check_not_empty({"users": 1, "organizations": 1}), [])


class Credentials(unittest.TestCase):
    def test_the_password_never_becomes_an_argument(self):
        # Two failures, one fix. `ps` shows argv to every local user, and a failed subprocess
        # raises an error whose text is the argument list, which on a public runner is a public
        # log. The job here uses a throwaway password; an operator running this by hand against
        # a scratch database on a real host would not be.
        captured = {}

        def fake_run(argv, **kwargs):
            captured["argv"] = argv
            captured["env"] = kwargs["env"]
            return subprocess.CompletedProcess(argv, 0, stdout="", stderr="")

        with unittest.mock.patch.object(subprocess, "run", fake_run):
            restore.Database("postgres://sotto:hunter2@db.example:5433/restored").rows("SELECT 1")

        self.assertNotIn("hunter2", " ".join(captured["argv"]))
        self.assertEqual(captured["env"]["PGPASSWORD"], "hunter2")
        self.assertEqual(captured["env"]["PGHOST"], "db.example")
        self.assertEqual(captured["env"]["PGPORT"], "5433")
        self.assertEqual(captured["env"]["PGDATABASE"], "restored")

    def test_a_url_with_no_credentials_still_works(self):
        env = restore.connection_env("postgres://localhost/restored")
        self.assertNotIn("PGPASSWORD", env)
        self.assertEqual(env["PGDATABASE"], "restored")

    def test_an_escaped_password_is_handed_over_decoded(self):
        env = restore.connection_env("postgres://u:p%40ss%3Aword@localhost/db")
        self.assertEqual(env["PGPASSWORD"], "p@ss:word")


class RepositoryMigrations(unittest.TestCase):
    def test_it_reads_the_versions_this_checkout_actually_carries(self):
        versions = restore.repository_migrations(str(ROOT / "crates/server/migrations"))
        self.assertEqual(versions[:3], ["1", "2", "3"])
        self.assertEqual(len(set(versions)), len(versions), "no version is counted twice")

    def test_nothing_but_a_numbered_migration_counts(self):
        with tempfile.TemporaryDirectory() as d:
            for name in ("0001_init.sql", "README.md", "notes.txt", "backup.sql.bak"):
                (Path(d) / name).write_text("")
            self.assertEqual(restore.repository_migrations(d), ["1"])


if __name__ == "__main__":
    unittest.main()
