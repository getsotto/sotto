"""Behaviour of scripts/backup-freshness, the verdict behind the Backup freshness workflow.

The bucket holds three kinds of object: the dump, its retention sidecar and, since backup.sh began
recording failures, a `sotto-<stamp>.failed.<step>` marker. Only a dump is a backup. These tests
pin that a marker can never make a stale bucket look healthy, and that the alarm's text tells the
two outages apart that look identical from outside: a job that failed and said where, and a job
that never reached the bucket at all, which is what one ten-day outage turned out to be.
"""

import json
import subprocess
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
SCRIPT = ROOT / "scripts/backup-freshness"
NOW = "2026-11-11T09:43:00Z"


def obj(name, created):
    return {"name": name, "timeCreated": created}


class Freshness(unittest.TestCase):
    def verdict(self, items, now=NOW, max_age=26):
        listing = {"items": items} if items is not None else {}
        result = subprocess.run(
            [str(SCRIPT), "--max-age-hours", str(max_age), "--now", now],
            input=json.dumps(listing), capture_output=True, text=True, timeout=30,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        out = dict(line.split("=", 1) for line in result.stdout.splitlines())
        return out, result.stderr

    def test_recent_dump_is_healthy(self):
        out, stderr = self.verdict([
            obj("sotto-20261111T021701Z.dump", "2026-11-11T02:17:20.618Z"),
            obj("sotto-20261111T021701Z.tombstones.jsonl", "2026-11-11T02:17:21.000Z"),
        ])
        self.assertEqual(out, {"healthy": "true"})
        self.assertIn("Newest backup: 2026-11-11T02:17:20.618Z (7h old, threshold 26h)", stderr)

    def test_fresh_marker_does_not_make_a_stale_bucket_healthy(self):
        # The regression this guards: the workflow used to take the newest object of any kind, so
        # a run that failed would have reported itself as the fresh backup it did not take.
        out, _ = self.verdict([
            obj("sotto-20261101T172800Z.dump", "2026-11-01T17:28:13.157Z"),
            obj("sotto-20261111T021701Z.failed.dumping-the-database", "2026-11-11T02:17:05.000Z"),
        ])
        self.assertEqual(out["healthy"], "false")
        self.assertIn("The most recent upload was 2026-11-01T17:28:13.157Z.", out["reason"])

    def test_stale_bucket_names_the_latest_failed_step(self):
        out, _ = self.verdict([
            obj("sotto-20261101T172800Z.dump", "2026-11-01T17:28:13.157Z"),
            obj("sotto-20261110T021701Z.failed.checking-tools", "2026-11-10T02:17:03.000Z"),
            obj("sotto-20261111T021701Z.failed.uploading-the-dump", "2026-11-11T02:17:40.000Z"),
        ])
        self.assertEqual(out["healthy"], "false")
        self.assertIn("recorded 2 failed runs since then", out["reason"])
        self.assertIn("at 2026-11-11T02:17:40.000Z, failed while uploading the dump", out["reason"])
        self.assertIn("step 6", out["reason"])

    def test_stale_bucket_without_markers_says_nothing_reached_it(self):
        out, _ = self.verdict([obj("sotto-20261101T172800Z.dump", "2026-11-01T17:28:13.157Z")])
        self.assertEqual(out["healthy"], "false")
        self.assertIn("past the 26 hour threshold", out["reason"])
        self.assertIn("recorded no failure since then", out["reason"])
        self.assertIn("deploy/.env", out["reason"])

    def test_failures_before_the_newest_dump_are_not_reported(self):
        # A failure that a later backup has already superseded is history, not the cause.
        out, _ = self.verdict([
            obj("sotto-20261102T021701Z.failed.dumping-the-database", "2026-11-02T02:17:05.000Z"),
            obj("sotto-20261103T021701Z.dump", "2026-11-03T02:17:20.000Z"),
        ])
        self.assertEqual(out["healthy"], "false")
        self.assertIn("recorded no failure since then", out["reason"])

    def test_only_backup_shaped_names_count(self):
        # Something else dropped in the bucket must not pass for a backup.
        out, _ = self.verdict([
            obj("sotto-20261101T172800Z.dump", "2026-11-01T17:28:13.157Z"),
            obj("notes.dump", "2026-11-11T09:00:00.000Z"),
            obj("sotto-latest.dump", "2026-11-11T09:00:00.000Z"),
        ])
        self.assertEqual(out["healthy"], "false")

    def test_a_prefix_in_the_destination_is_allowed(self):
        out, _ = self.verdict([obj("prod/sotto-20261111T021701Z.dump", "2026-11-11T02:17:20.000Z")])
        self.assertEqual(out, {"healthy": "true"})

    def test_marker_step_outside_the_expected_wording_is_ignored(self):
        # The step reaches an issue body. A name with anything but lowercase words and hyphens is
        # not one backup.sh writes, so it is not read as one.
        out, _ = self.verdict([
            obj("sotto-20261101T172800Z.dump", "2026-11-01T17:28:13.157Z"),
            obj("sotto-20261111T021701Z.failed.[click](https://x.test)", "2026-11-11T02:17:05.000Z"),
        ])
        self.assertIn("recorded no failure since then", out["reason"])
        self.assertNotIn("x.test", out["reason"])

    def test_empty_bucket(self):
        out, _ = self.verdict(None)
        self.assertEqual(out["healthy"], "false")
        self.assertIn("holds no backup", out["reason"])

    def test_threshold_boundary(self):
        # 26 whole hours old is a breach: one missed night has to land on the alarm, not under it.
        items = [obj("sotto-20261110T074300Z.dump", "2026-11-10T07:43:00.000Z")]
        self.assertEqual(self.verdict(items)[0]["healthy"], "false")
        items = [obj("sotto-20261110T074400Z.dump", "2026-11-10T07:44:00.000Z")]
        self.assertEqual(self.verdict(items)[0]["healthy"], "true")

    def test_reason_is_one_line(self):
        # GITHUB_OUTPUT takes key=value per line; a newline in the reason would cut it short.
        out, _ = self.verdict([
            obj("sotto-20261101T172800Z.dump", "2026-11-01T17:28:13.157Z"),
            obj("sotto-20261111T021701Z.failed.uploading-the-dump", "2026-11-11T02:17:40.000Z"),
        ])
        self.assertNotIn("\n", out["reason"])


if __name__ == "__main__":
    unittest.main()
