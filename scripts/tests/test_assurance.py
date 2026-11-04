"""Tests for the same-run assurance check validator."""

import importlib.machinery
import importlib.util
from io import BytesIO
import json
import os
from pathlib import Path
import tempfile
import unittest
from urllib.error import HTTPError
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

    def test_queued_workflow_passes_only_when_required_jobs_pass(self):
        manifest = assurance.load_manifest()
        names = manifest["groups"]["codec"]["required_jobs"]
        payload = fixture("codec", names)
        payload["run"]["status"] = "queued"

        verdict = assurance.validate_run(manifest, "codec", payload["run"], payload["jobs"])
        self.assertEqual(verdict["status"], "passed")

        payload["jobs"][0]["conclusion"] = None
        with self.assertRaisesRegex(assurance.AssuranceError, "concluded None"):
            assurance.validate_run(manifest, "codec", payload["run"], payload["jobs"])

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

    def test_remote_validation_retries_until_github_publishes_job_conclusions(self):
        manifest = assurance.load_manifest()
        names = manifest["groups"]["kani"]["required_jobs"]
        incomplete = (fixture("kani", names)["run"], fixture("kani", names, {names[0]: None})["jobs"])
        complete = (fixture("kani", names)["run"], fixture("kani", names)["jobs"])
        with (
            patch.dict(
                os.environ,
                {
                    "GITHUB_RUN_ID": "42",
                    "GITHUB_RUN_ATTEMPT": "1",
                    "GITHUB_REPOSITORY": "getsotto/sotto",
                    "GITHUB_TOKEN": "token",
                },
            ),
            patch.object(assurance, "fetch_run", side_effect=[incomplete, complete]) as fetch,
            patch.object(assurance.time, "sleep") as sleep,
        ):
            self.assertEqual(assurance.main(["--group", "kani", "--expected-sha", "abc123"]), 0)
        self.assertEqual(fetch.call_count, 2)
        sleep.assert_called_once_with(assurance.ASSURANCE_STATUS_RETRY_DELAYS[0])

    def test_remote_validation_retries_when_github_temporarily_omits_a_job(self):
        manifest = assurance.load_manifest()
        names = manifest["groups"]["kani"]["required_jobs"]
        missing = (fixture("kani", names)["run"], [])
        complete = (fixture("kani", names)["run"], fixture("kani", names)["jobs"])
        with (
            patch.dict(
                os.environ,
                {
                    "GITHUB_RUN_ID": "42",
                    "GITHUB_RUN_ATTEMPT": "1",
                    "GITHUB_REPOSITORY": "getsotto/sotto",
                    "GITHUB_TOKEN": "token",
                },
            ),
            patch.object(assurance, "fetch_run", side_effect=[missing, complete]) as fetch,
            patch.object(assurance.time, "sleep") as sleep,
        ):
            self.assertEqual(assurance.main(["--group", "kani", "--expected-sha", "abc123"]), 0)
        self.assertEqual(fetch.call_count, 2)
        sleep.assert_called_once_with(assurance.ASSURANCE_STATUS_RETRY_DELAYS[0])

    def test_remote_validation_fails_closed_after_bounded_incomplete_results(self):
        manifest = assurance.load_manifest()
        names = manifest["groups"]["kani"]["required_jobs"]
        incomplete = (fixture("kani", names)["run"], fixture("kani", names, {names[0]: None})["jobs"])
        with (
            patch.dict(
                os.environ,
                {
                    "GITHUB_RUN_ID": "42",
                    "GITHUB_RUN_ATTEMPT": "1",
                    "GITHUB_REPOSITORY": "getsotto/sotto",
                    "GITHUB_TOKEN": "token",
                },
            ),
            patch.object(
                assurance,
                "fetch_run",
                side_effect=[incomplete] * (len(assurance.ASSURANCE_STATUS_RETRY_DELAYS) + 1),
            ) as fetch,
            patch.object(assurance.time, "sleep") as sleep,
        ):
            self.assertEqual(assurance.main(["--group", "kani", "--expected-sha", "abc123"]), 1)
        self.assertEqual(fetch.call_count, len(assurance.ASSURANCE_STATUS_RETRY_DELAYS) + 1)
        self.assertEqual(
            [call.args[0] for call in sleep.call_args_list],
            list(assurance.ASSURANCE_STATUS_RETRY_DELAYS),
        )

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

    def test_get_json_retries_transient_server_errors(self):
        response = BytesIO(b'{"ok": true}')
        transient = HTTPError("https://api.github.com/example", 502, "Bad Gateway", {}, None)
        with patch.object(assurance, "urlopen", side_effect=[transient, response]) as open_url:
            with patch.object(assurance.time, "sleep") as sleep:
                result = assurance._get_json("https://api.github.com/example", "token")
        self.assertEqual(result, {"ok": True})
        self.assertEqual(open_url.call_count, 2)
        request = open_url.call_args_list[0].args[0]
        self.assertEqual(request.get_header("Cache-control"), "no-cache")
        sleep.assert_called_once_with(1)

    def test_get_json_does_not_retry_permanent_client_errors(self):
        permanent = HTTPError("https://api.github.com/example", 400, "Bad Request", {}, None)
        with patch.object(assurance, "urlopen", side_effect=permanent) as open_url:
            with patch.object(assurance.time, "sleep") as sleep:
                with self.assertRaisesRegex(assurance.AssuranceError, "HTTP Error 400"):
                    assurance._get_json("https://api.github.com/example", "token")
        open_url.assert_called_once()
        sleep.assert_not_called()

    def test_get_json_stops_after_bounded_transient_retries(self):
        transient_errors = [
            HTTPError("https://api.github.com/example", 502, "Bad Gateway", {}, None)
            for _ in range(len(assurance.API_RETRY_DELAYS) + 1)
        ]
        with patch.object(assurance, "urlopen", side_effect=transient_errors) as open_url:
            with patch.object(assurance.time, "sleep") as sleep:
                with self.assertRaisesRegex(assurance.AssuranceError, "HTTP Error 502"):
                    assurance._get_json("https://api.github.com/example", "token")
        self.assertEqual(open_url.call_count, len(assurance.API_RETRY_DELAYS) + 1)
        self.assertEqual(
            [call.args[0] for call in sleep.call_args_list], list(assurance.API_RETRY_DELAYS)
        )

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
