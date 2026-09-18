"""Failure-contract tests for the core fuzz campaign wrapper."""

import importlib.machinery
import importlib.util
import json
from pathlib import Path
import shutil
from types import SimpleNamespace
import tempfile
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[2]
TEST_TARGET_ROOT = ROOT / "target"
TEST_TARGET_ROOT.mkdir(exist_ok=True)
loader = importlib.machinery.SourceFileLoader("core_fuzz", str(ROOT / "scripts/check-core-fuzz"))
spec = importlib.util.spec_from_loader(loader.name, loader)
runner = importlib.util.module_from_spec(spec)
loader.exec_module(runner)


class ParserTests(unittest.TestCase):
    def test_requires_profile_and_target(self):
        with self.assertRaises(SystemExit):
            runner.parse_args([])

    def test_execution_marker_is_parsed(self):
        self.assertEqual(runner._executions("#1 INITED\nDone 42 runs in 30 second(s)\n"), 42)
        self.assertEqual(runner._completion("Done 42 runs in 30 second(s)\n"), (42, 30.0))

    def test_completion_rejects_trailing_text(self):
        self.assertEqual(
            runner._completion("Done 42 runs in 30 second(s) but the campaign stopped\n"),
            (None, None),
        )

    def test_missing_marker_is_inconclusive(self):
        self.assertEqual(runner._executions("#1 INITED\n"), 0)

    def test_unknown_target_rejected(self):
        with self.assertRaises(SystemExit):
            runner.parse_args(["--profile", "pr", "--target", "other"])

    def test_unknown_profile_rejected(self):
        with self.assertRaises(SystemExit):
            runner.parse_args(["--profile", "weekly", "--target", "base32_codec"])

    def test_seed_can_be_selected_explicitly(self):
        args = runner.parse_args(["--profile", "pr", "--target", "base32_codec", "--seed", "0xbeef"])
        self.assertEqual(args.seed, 0xBEEF)

    def test_optional_corpus_can_be_selected(self):
        args = runner.parse_args(["--profile", "pr", "--target", "base32_codec", "--corpus", "target/corpus"])
        self.assertEqual(args.corpus, Path("target/corpus"))

    def test_nightly_seed_is_fresh_when_unconfigured(self):
        with patch.object(runner.secrets, "randbelow", return_value=0xBEEE):
            self.assertEqual(runner.resolve_seed("nightly"), 0xBEEF)

    def test_pr_seed_is_stable_when_unconfigured(self):
        self.assertEqual(runner.resolve_seed("pr"), runner.FUZZ_SEED)

    def test_large_workflow_seed_is_folded_into_libfuzzer_range(self):
        with patch.dict(runner.os.environ, {"CORE_FUZZ_SEED": "35002964852"}):
            seed = runner.resolve_seed("nightly")
        self.assertGreater(seed, 0)
        self.assertLessEqual(seed, runner.MAX_FUZZ_SEED)


class EvidenceTests(unittest.TestCase):
    def test_initial_status_is_failed(self):
        evidence = runner.new_evidence("pr", "base32_codec", ROOT / "target" / "core-fuzz" / "x", "address")
        self.assertEqual(evidence["status"], "failed")

    def test_profiles_have_explicit_budgets(self):
        self.assertEqual(runner.PROFILES, {"pr": 30, "nightly": 1800})

    def test_targets_are_explicit(self):
        self.assertEqual(runner.TARGETS, {"base32_codec", "key_strings"})

    def test_config_declares_the_runner_pins(self):
        config = __import__("json").loads((ROOT / "fuzz" / "config.json").read_text())
        self.assertEqual(config["cargo_fuzz"], "0.13.2")
        self.assertEqual(config["rust_toolchain"], runner.TOOLCHAIN)
        self.assertEqual(config["targets"], sorted(runner.TARGETS))
        self.assertEqual(config["max_input_bytes"], runner.MAX_LEN)

    def test_corpus_digest_frames_paths_and_contents(self):
        with tempfile.TemporaryDirectory(dir=TEST_TARGET_ROOT) as first, tempfile.TemporaryDirectory(dir=TEST_TARGET_ROOT) as second:
            first_path, second_path = Path(first), Path(second)
            (first_path / "a").write_bytes(b"bc")
            (second_path / "ab").write_bytes(b"c")
            self.assertNotEqual(runner.sha256_tree(first_path), runner.sha256_tree(second_path))

    def test_evidence_records_reproducibility_metadata(self):
        evidence = runner.new_evidence("pr", "base32_codec", ROOT / "target" / "core-fuzz" / "x", "address")
        for field in ("config_sha256", "lockfile_sha256", "starting_corpus_sha256", "workflow", "outcome", "rng_seed"):
            self.assertIn(field, evidence)
        self.assertNotEqual(evidence["rng_seed"], 0)

    def test_campaign_replays_seeds_and_sets_timeout(self):
        result = SimpleNamespace(returncode=0, stdout="Done 1 runs in 30 second(s)\n", stderr="")
        with tempfile.TemporaryDirectory(dir=TEST_TARGET_ROOT) as directory:
            output = Path(directory)
            evidence = runner.new_evidence("pr", "base32_codec", output, "none")
            with patch.object(runner, "command", return_value=result) as mocked:
                runner.run_campaign("pr", "base32_codec", output, evidence, "none")
            self.assertEqual(evidence["status"], "passed")
            self.assertTrue(any("-timeout=10" in call.args[0] for call in mocked.call_args_list))
            self.assertTrue(any("-seed=23063" in call.args[0] for call in mocked.call_args_list))
            self.assertTrue(evidence["seed_replay"])

    def test_campaign_adds_optional_corpus_to_fresh_working_copy(self):
        result = SimpleNamespace(returncode=0, stdout="Done 1 runs in 30 second(s)\n", stderr="")
        with tempfile.TemporaryDirectory(dir=TEST_TARGET_ROOT) as directory:
            output = Path(directory)
            restored = output / "restored"
            (restored / "inputs").mkdir(parents=True)
            generated = restored / "inputs" / "generated"
            generated.write_bytes(b"seed")
            (restored / "manifest.json").write_text(
                json.dumps(
                    {
                        "schema_version": 1,
                        "format": runner.CORPUS_FORMAT,
                        "target": "base32_codec",
                        "files": [{"path": "inputs/generated", "sha256": runner.sha256_file(generated)}],
                    }
                ),
                encoding="utf-8",
            )
            evidence = runner.new_evidence("pr", "base32_codec", output, "none", corpus_source=restored)
            with patch.object(runner, "command", return_value=result):
                runner.run_campaign("pr", "base32_codec", output, evidence, "none", corpus_source=restored)
            self.assertEqual(next((output / "corpus" / "restored").iterdir()).read_bytes(), b"seed")
            self.assertIsNotNone(evidence["campaign_corpus_sha256"])
            self.assertTrue(any(item["input"].startswith("restored:") for item in evidence["seed_replay"]))

    def test_invalid_restored_corpus_manifest_fails(self):
        with tempfile.TemporaryDirectory(dir=TEST_TARGET_ROOT) as directory:
            output = Path(directory)
            restored = output / "restored"
            restored.mkdir()
            (restored / "manifest.json").write_text("{}", encoding="utf-8")
            evidence = runner.new_evidence("pr", "base32_codec", output, "none", corpus_source=restored)
            with patch.object(runner, "command") as mocked:
                with self.assertRaises(runner.CampaignError):
                    runner.run_campaign("pr", "base32_codec", output, evidence, "none", corpus_source=restored)
            mocked.assert_not_called()

    def test_restored_corpus_hash_mismatch_fails(self):
        with tempfile.TemporaryDirectory(dir=TEST_TARGET_ROOT) as directory:
            output = Path(directory)
            restored = output / "restored"
            (restored / "inputs").mkdir(parents=True)
            generated = restored / "inputs" / "generated"
            generated.write_bytes(b"seed")
            (restored / "manifest.json").write_text(
                json.dumps(
                    {
                        "schema_version": 1,
                        "format": runner.CORPUS_FORMAT,
                        "target": "base32_codec",
                        "files": [{"path": "inputs/generated", "sha256": "0" * 64}],
                    }
                ),
                encoding="utf-8",
            )
            evidence = runner.new_evidence("pr", "base32_codec", output, "none", corpus_source=restored)
            with patch.object(runner, "command") as mocked:
                with self.assertRaises(runner.CampaignError):
                    runner.run_campaign("pr", "base32_codec", output, evidence, "none", corpus_source=restored)
            mocked.assert_not_called()

    def test_failed_seed_replay_does_not_pass(self):
        build = SimpleNamespace(returncode=0, stdout="built", stderr="")
        failed = SimpleNamespace(returncode=7, stdout="replay stdout", stderr="sanitizer crash")
        with tempfile.TemporaryDirectory(dir=TEST_TARGET_ROOT) as directory:
            output = Path(directory)
            evidence = runner.new_evidence("pr", "base32_codec", output, "none")
            with patch.object(runner, "command", side_effect=[build, failed]):
                with self.assertRaises(runner.CampaignError):
                    runner.run_campaign("pr", "base32_codec", output, evidence, "none")
            self.assertEqual(evidence["status"], "failed")
            replay = evidence["seed_replay"][-1]
            self.assertEqual(replay["exit_code"], 7)
            self.assertIn("-runs=1", replay["command"])
            self.assertEqual((output / replay["stdout_log"]).read_text(), "replay stdout")
            self.assertEqual((output / replay["stderr_log"]).read_text(), "sanitizer crash")

    def test_seed_replay_timeout_retains_partial_output(self):
        import subprocess

        build = SimpleNamespace(returncode=0, stdout="built", stderr="")
        timeout = subprocess.TimeoutExpired("cargo", 120, output=b"partial stdout", stderr="partial stderr")
        with tempfile.TemporaryDirectory(dir=TEST_TARGET_ROOT) as directory:
            output = Path(directory)
            evidence = runner.new_evidence("pr", "base32_codec", output, "none")
            with patch.object(runner, "command", side_effect=[build, timeout]):
                with self.assertRaises(subprocess.TimeoutExpired):
                    runner.run_campaign("pr", "base32_codec", output, evidence, "none")
            replay = evidence["seed_replay"][-1]
            self.assertTrue(replay["timed_out"])
            self.assertTrue(replay["input"].startswith("tracked:"))
            self.assertIn("-runs=1", replay["command"])
            self.assertEqual((output / replay["stdout_log"]).read_text(), "partial stdout")
            self.assertEqual((output / replay["stderr_log"]).read_text(), "partial stderr")

    def test_campaign_timeout_does_not_pass(self):
        import subprocess

        with tempfile.TemporaryDirectory(dir=TEST_TARGET_ROOT) as directory:
            output = Path(directory)
            evidence = runner.new_evidence("pr", "base32_codec", output, "none")
            with patch.object(runner, "command", side_effect=subprocess.TimeoutExpired("cargo", 30)):
                with self.assertRaises(subprocess.TimeoutExpired):
                    runner.run_campaign("pr", "base32_codec", output, evidence, "none")
            self.assertEqual(evidence["status"], "failed")

    def test_failing_replay_returns_campaign_error(self):
        result = SimpleNamespace(returncode=1, stdout="", stderr="crash")
        with tempfile.TemporaryDirectory(dir=TEST_TARGET_ROOT) as directory:
            output = Path(directory)
            replay = output / "reproducer"
            replay.write_bytes(b"crash")
            evidence = runner.new_evidence("pr", "base32_codec", output, "none")
            with patch.object(runner, "command", return_value=result):
                with self.assertRaises(runner.CampaignError):
                    runner.run_campaign("pr", "base32_codec", output, evidence, "none", replay)
            self.assertEqual(evidence["status"], "failed")

    def test_campaign_without_completion_marker_does_not_pass(self):
        result = SimpleNamespace(returncode=0, stdout="#1 INITED\n", stderr="")
        with tempfile.TemporaryDirectory(dir=TEST_TARGET_ROOT) as directory:
            output = Path(directory)
            evidence = runner.new_evidence("pr", "base32_codec", output, "none")
            with patch.object(runner, "command", return_value=result):
                with self.assertRaises(runner.CampaignError):
                    runner.run_campaign("pr", "base32_codec", output, evidence, "none")
            self.assertEqual(evidence["status"], "failed")

    def test_campaign_records_incomplete_outcome_for_zero_runs(self):
        result = SimpleNamespace(returncode=0, stdout="Done 0 runs in 0 second(s)\n", stderr="")
        with tempfile.TemporaryDirectory(dir=TEST_TARGET_ROOT) as directory:
            output = Path(directory)
            evidence = runner.new_evidence("pr", "base32_codec", output, "none")
            with patch.object(runner, "command", return_value=result):
                with self.assertRaises(runner.CampaignError):
                    runner.run_campaign("pr", "base32_codec", output, evidence, "none")
            self.assertEqual(evidence["outcome"], "incomplete_campaign")

    def test_campaign_shorter_than_budget_does_not_pass(self):
        result = SimpleNamespace(returncode=0, stdout="Done 1 runs in 1 second(s)\n", stderr="")
        with tempfile.TemporaryDirectory(dir=TEST_TARGET_ROOT) as directory:
            output = Path(directory)
            evidence = runner.new_evidence("pr", "base32_codec", output, "none")
            with patch.object(runner, "command", return_value=result):
                with self.assertRaises(runner.CampaignError):
                    runner.run_campaign("pr", "base32_codec", output, evidence, "none")
            self.assertEqual(evidence["outcome"], "incomplete_campaign")

    def test_campaign_allows_one_second_marker_rounding(self):
        build = SimpleNamespace(returncode=0, stdout="", stderr="")
        replay = SimpleNamespace(returncode=0, stdout="", stderr="")
        campaign = SimpleNamespace(returncode=0, stdout="Done 1 runs in 29 second(s)\n", stderr="")

        def command_result(args, **_kwargs):
            return campaign if any("-max_total_time=" in arg for arg in args) else (build if "build" in args else replay)

        with tempfile.TemporaryDirectory(dir=TEST_TARGET_ROOT) as directory:
            output = Path(directory)
            evidence = runner.new_evidence("pr", "base32_codec", output, "none")
            with patch.object(runner, "command", side_effect=command_result):
                runner.run_campaign("pr", "base32_codec", output, evidence, "none")
            self.assertEqual(evidence["status"], "passed")

    def test_failure_outcome_distinguishes_sanitizer_and_interruption(self):
        sanitizer = SimpleNamespace(returncode=1, stdout="", stderr="AddressSanitizer: heap-use-after-free")
        interrupted = SimpleNamespace(returncode=-9, stdout="", stderr="")
        self.assertEqual(runner.failure_outcome(sanitizer), "sanitizer_failure")
        self.assertEqual(runner.failure_outcome(interrupted), "interrupted")

    def test_campaign_signal_does_not_pass(self):
        replay_ok = SimpleNamespace(returncode=0, stdout="Done 1 runs in 30 second(s)\n", stderr="")
        campaign_signal = SimpleNamespace(returncode=-9, stdout="", stderr="")

        def command_result(args, **_):
            return campaign_signal if any("-max_total_time=" in arg for arg in args) else replay_ok

        with tempfile.TemporaryDirectory(dir=TEST_TARGET_ROOT) as directory:
            output = Path(directory)
            evidence = runner.new_evidence("pr", "base32_codec", output, "none")
            with patch.object(runner, "command", side_effect=command_result):
                with self.assertRaises(runner.CampaignError):
                    runner.run_campaign("pr", "base32_codec", output, evidence, "none")
            self.assertEqual(evidence["outcome"], "interrupted")

    def test_campaign_crash_does_not_pass(self):
        replay_ok = SimpleNamespace(returncode=0, stdout="Done 1 runs in 30 second(s)\n", stderr="")
        campaign_crash = SimpleNamespace(returncode=1, stdout="", stderr="panicked at fuzz target")

        def command_result(args, **_):
            return campaign_crash if any("-max_total_time=" in arg for arg in args) else replay_ok

        with tempfile.TemporaryDirectory(dir=TEST_TARGET_ROOT) as directory:
            output = Path(directory)
            evidence = runner.new_evidence("pr", "base32_codec", output, "none")
            with patch.object(runner, "command", side_effect=command_result):
                with self.assertRaises(runner.CampaignError):
                    runner.run_campaign("pr", "base32_codec", output, evidence, "none")
            self.assertEqual(evidence["outcome"], "assertion_failure")

    def _assert_seed_validation_failure(self, extra_args, env_seed, suffix, error_text):
        output = ROOT / "target" / "core-fuzz" / f"base32_codec-{suffix}"
        shutil.rmtree(output, ignore_errors=True)
        self.addCleanup(shutil.rmtree, output, True)
        argv = ["--profile", "pr", "--target", "base32_codec", *extra_args]
        with patch.object(runner.uuid, "uuid4", return_value=SimpleNamespace(hex=suffix)), patch.dict(
            runner.os.environ, {"CORE_FUZZ_SEED": env_seed}
        ), patch.object(runner, "ensure_pins") as ensure_pins, patch.object(runner, "run_campaign") as run_campaign:
            self.assertEqual(runner.main(argv), 1)
        ensure_pins.assert_not_called()
        run_campaign.assert_not_called()
        evidence = json.loads((output / "run.json").read_text(encoding="utf-8"))
        self.assertEqual(evidence["profile"], "pr")
        self.assertEqual(evidence["target"], "base32_codec")
        self.assertEqual(evidence["status"], "failed")
        self.assertEqual(evidence["outcome"], "seed_validation_failure")
        self.assertIsNone(evidence["rng_seed"])
        self.assertIn(error_text, evidence["error"])
        self.assertNotIn("command", evidence)
        self.assertNotIn("rustc_version", evidence)

    def test_zero_seed_writes_failed_evidence_without_starting_campaign(self):
        self._assert_seed_validation_failure(["--seed", "0"], "123", "seedzero", "between 1 and")

    def test_negative_seed_writes_failed_evidence_without_starting_campaign(self):
        self._assert_seed_validation_failure(["--seed", "-7"], "123", "seednegative", "between 1 and")

    def test_malformed_environment_seed_writes_failed_evidence_without_starting_campaign(self):
        self._assert_seed_validation_failure([], "not-an-integer", "seedenvironment", "must be an integer")

    def test_failed_final_evidence_write_does_not_pass(self):
        writes = 0

        def write_evidence(path, evidence):
            nonlocal writes
            writes += 1
            if writes == 2:
                raise OSError("disk full")

        with patch.object(runner, "write_evidence", side_effect=write_evidence), patch.object(
            runner, "ensure_pins", return_value="rustc test"
        ), patch.object(runner, "run_campaign"):
            self.assertEqual(runner.main(["--profile", "pr", "--target", "base32_codec"]), 1)


if __name__ == "__main__":
    unittest.main()
