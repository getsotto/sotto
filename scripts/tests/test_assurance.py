"""Tests for the same-run assurance check validator."""

import importlib.machinery
import importlib.util
import json
import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch


ROOT = Path(__file__).resolve().parents[2]
TEST_TARGET_ROOT = ROOT / "target"
TEST_TARGET_ROOT.mkdir(exist_ok=True)
loader = importlib.machinery.SourceFileLoader("assurance", str(ROOT / "scripts/check-assurance"))
spec = importlib.util.spec_from_loader(loader.name, loader)
assurance = importlib.util.module_from_spec(spec)
loader.exec_module(assurance)


def fixture(group, names, conclusions=None):
    conclusions = conclusions or {name: "success" for name in names}
    return {
        "run": {
            "id": 42,
            "status": "in_progress",
            "run_attempt": 1,
            "head_sha": "abc123",
            "workflow_path": assurance.load_manifest()["groups"][group]["workflow"],
        },
        "jobs": [{"name": name, "conclusion": conclusions.get(name)} for name in names],
    }


class AssuranceTests(unittest.TestCase):
    def test_complete_run_passes(self):
        manifest = assurance.load_manifest()
        names = manifest["groups"]["kani"]["required_jobs"]
        verdict = assurance.validate_run(manifest, "kani", fixture("kani", names)["run"], fixture("kani", names)["jobs"], expected_sha="abc123", expected_attempt=1)
        self.assertEqual(verdict["status"], "passed")

    def test_missing_job_fails(self):
        manifest = assurance.load_manifest()
        payload = fixture("kani", [])
        with self.assertRaisesRegex(assurance.AssuranceError, "missing required jobs"):
            assurance.validate_run(manifest, "kani", payload["run"], payload["jobs"])

    def test_skipped_job_fails(self):
        manifest = assurance.load_manifest()
        names = manifest["groups"]["kani"]["required_jobs"]
        payload = fixture("kani", names, {names[0]: "skipped"})
        with self.assertRaisesRegex(assurance.AssuranceError, "skipped"):
            assurance.validate_run(manifest, "kani", payload["run"], payload["jobs"])

    def test_intentionally_skipped_profile_is_ignored(self):
        manifest = assurance.load_manifest()
        names = manifest["groups"]["codec"]["required_jobs"]
        ignored = manifest["groups"]["codec"]["ignored_jobs"][1]
        payload = fixture("codec", names + [ignored], {**{name: "success" for name in names}, ignored: "skipped"})
        verdict = assurance.validate_run(manifest, "codec", payload["run"], payload["jobs"])
        self.assertEqual(verdict["status"], "passed")

    def test_duplicate_and_unexpected_jobs_fail(self):
        manifest = assurance.load_manifest()
        names = manifest["groups"]["kani"]["required_jobs"]
        payload = fixture("kani", names + names + ["unrelated"])
        with self.assertRaisesRegex(assurance.AssuranceError, "duplicate job names"):
            assurance.validate_run(manifest, "kani", payload["run"], payload["jobs"])

    def test_wrong_source_and_attempt_fail(self):
        manifest = assurance.load_manifest()
        names = manifest["groups"]["kani"]["required_jobs"]
        payload = fixture("kani", names)
        with self.assertRaisesRegex(assurance.AssuranceError, "source"):
            assurance.validate_run(manifest, "kani", payload["run"], payload["jobs"], expected_sha="different", expected_attempt=1)
        with self.assertRaisesRegex(assurance.AssuranceError, "attempt"):
            assurance.validate_run(manifest, "kani", payload["run"], payload["jobs"], expected_sha="abc123", expected_attempt=2)

    def test_jobs_file_path_produces_a_verdict(self):
        with tempfile.TemporaryDirectory(dir=TEST_TARGET_ROOT) as directory:
            path = Path(directory) / "run.json"
            names = assurance.load_manifest()["groups"]["kani"]["required_jobs"]
            path.write_text(json.dumps(fixture("kani", names)), encoding="utf-8")
            # --run-attempt defaults from GITHUB_RUN_ATTEMPT, so pin it: a re-run
            # workflow would otherwise compare the fixture's attempt 1 against 2+.
            with patch.dict(os.environ, {"GITHUB_RUN_ATTEMPT": "1"}):
                argv = ["--group", "kani", "--jobs-file", str(path), "--expected-sha", "abc123"]
                self.assertEqual(assurance.main(argv), 0)

    def test_fetch_uses_latest_attempt_and_paginates(self):
        first_page = [{"name": f"job-{index}", "conclusion": "success"} for index in range(100)]
        second_page = [{"name": "job-100", "conclusion": "success"}]
        responses = [
            {"id": 42, "status": "completed", "run_attempt": 2, "head_sha": "abc123", "path": ".github/workflows/kani.yml"},
            {"jobs": first_page},
            {"jobs": second_page},
        ]
        with patch.object(assurance, "_get_json", side_effect=responses) as get_json:
            run, jobs = assurance.fetch_run("getsotto/sotto", 42, "token")
        self.assertEqual(run["run_attempt"], 2)
        self.assertEqual(jobs, first_page + second_page)
        self.assertEqual(get_json.call_args_list[1].args[0], "https://api.github.com/repos/getsotto/sotto/actions/runs/42/jobs?filter=latest&per_page=100&page=1")
        self.assertEqual(get_json.call_args_list[2].args[0], "https://api.github.com/repos/getsotto/sotto/actions/runs/42/jobs?filter=latest&per_page=100&page=2")

    def test_fetch_exact_multiple_requests_empty_final_page(self):
        page = [{"name": f"job-{index}", "conclusion": "success"} for index in range(100)]
        responses = [
            {"id": 42, "status": "completed", "run_attempt": 1, "head_sha": "abc123", "path": ".github/workflows/kani.yml"},
            {"jobs": page},
            {"jobs": []},
        ]
        with patch.object(assurance, "_get_json", side_effect=responses) as get_json:
            _, jobs = assurance.fetch_run("getsotto/sotto", 42, "token")
        self.assertEqual(jobs, page)
        self.assertEqual(len(get_json.call_args_list), 3)
        self.assertIn("page=2", get_json.call_args_list[2].args[0])

    def test_second_page_required_failure_reaches_validator(self):
        manifest = {
            "groups": {
                "test": {
                    "workflow": ".github/workflows/test.yml",
                    "completion": "assurance complete",
                    "required_jobs": ["first", "required-second"],
                    "ignored_jobs": ["ignored"],
                }
            }
        }
        run_response = {"id": 42, "status": "completed", "run_attempt": 1, "head_sha": "abc123", "path": ".github/workflows/test.yml"}
        filler = [{"name": "ignored", "conclusion": "success"} for _ in range(98)]
        first_page = [{"name": "first", "conclusion": "success"}, *filler, {"name": "assurance complete", "conclusion": "success"}]

        for conclusion, should_pass in (("failure", False), ("success", True)):
            with self.subTest(conclusion=conclusion):
                responses = [run_response, {"jobs": first_page}, {"jobs": [{"name": "required-second", "conclusion": conclusion}]}]
                with patch.object(assurance, "_get_json", side_effect=responses):
                    run, jobs = assurance.fetch_run("getsotto/sotto", 42, "token")
                if should_pass:
                    verdict = assurance.validate_run(manifest, "test", run, jobs)
                    self.assertEqual(verdict["status"], "passed")
                else:
                    with self.assertRaisesRegex(assurance.AssuranceError, "required-second concluded 'failure'"):
                        assurance.validate_run(manifest, "test", run, jobs)


if __name__ == "__main__":
    unittest.main()
