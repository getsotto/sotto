from contextlib import redirect_stderr, redirect_stdout
from importlib.machinery import SourceFileLoader
import io
import os
import signal
import subprocess
import sys
import tempfile
from pathlib import Path
import unittest
from unittest import mock


SCRIPT = Path(__file__).resolve().parents[1] / "check-cloud-coverage-races"
MODULE = SourceFileLoader("cloud_coverage_races", str(SCRIPT)).load_module()

#: Helper that records its own and its child's pid, emits text and byte payloads,
#: then sleeps past the runner timeout. Takes the pid file path as argv[1].
SPAWNING_SLEEPER = "; ".join(
    [
        "import os, subprocess, sys, time",
        "child = subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(60)'])",
        "open(sys.argv[1], 'w').write(f'{os.getpid()} {child.pid}')",
        "sys.stdout.buffer.write(b'partial-stdout \\xff\\n'); sys.stdout.flush()",
        "sys.stderr.write('partial-stderr\\n'); sys.stderr.flush()",
        "time.sleep(60)",
    ]
)

#: Same shape, but parent and child ignore SIGTERM so only SIGKILL can reap them.
TERM_IGNORING_SLEEPER = "; ".join(
    [
        "import os, signal, subprocess, sys, time",
        "signal.signal(signal.SIGTERM, signal.SIG_IGN)",
        "child = subprocess.Popen([sys.executable, '-c', "
        "'import signal, time; "
        "signal.signal(signal.SIGTERM, signal.SIG_IGN); time.sleep(60)'])",
        "open(sys.argv[1], 'w').write(f'{os.getpid()} {child.pid}')",
        "time.sleep(60)",
    ]
)


def read_pids(pid_file):
    with open(pid_file, encoding="utf-8") as handle:
        return [int(part) for part in handle.read().split()]


def process_exists(pid):
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    except PermissionError:
        return True
    return True


def kill_best_effort(pid_file):
    try:
        pids = read_pids(pid_file)
    except OSError:
        return
    for pid in pids:
        try:
            os.kill(pid, signal.SIGKILL)
        except OSError:
            pass


class CloudCoverageRaceRunnerTests(unittest.TestCase):
    def test_requires_explicit_database_opt_in(self):
        with self.assertRaisesRegex(ValueError, "SOTTO_RUN_DB_TESTS"):
            MODULE.validate_database({"DATABASE_URL": "postgres://localhost/sotto"})

    def test_rejects_remote_database(self):
        with self.assertRaisesRegex(ValueError, "local disposable"):
            MODULE.validate_database(
                {"SOTTO_RUN_DB_TESTS": "1", "DATABASE_URL": "postgres://db.example/sotto"}
            )

    def test_rejects_query_parameters(self):
        with self.assertRaisesRegex(ValueError, "local disposable"):
            MODULE.validate_database(
                {
                    "SOTTO_RUN_DB_TESTS": "1",
                    "DATABASE_URL": "postgres://localhost/sotto?sslmode=disable",
                }
            )

    def test_accepts_local_disposable_database(self):
        MODULE.validate_database(
            {"SOTTO_RUN_DB_TESTS": "1", "DATABASE_URL": "postgres://localhost/sotto"}
        )


class DiscoveryTests(unittest.TestCase):
    """Manifest and selection checks without starting Cargo."""

    def listed(self, stdout):
        completed = subprocess.CompletedProcess(
            ["cargo", "test", "--", "--list"], 0, stdout, ""
        )
        with mock.patch.object(MODULE, "run", return_value=completed) as runner:
            names = MODULE.discover("cloud_coverage_store", {}, 30)
        runner.assert_called_once()
        return names

    def test_discover_parses_listed_tests(self):
        self.assertEqual(
            self.listed("alpha: test\nbeta: test\ntest result: ok. 2 listed\n"),
            {"alpha", "beta"},
        )

    def test_discover_with_zero_tests_selects_nothing(self):
        self.assertEqual(self.listed(""), set())

    def test_manifest_rejects_misspelled_name(self):
        discovered = {}
        for target, name in MODULE.SCENARIOS:
            discovered.setdefault(target, set()).add(name)
        victim_target, victim_name = MODULE.SCENARIOS[0]
        discovered[victim_target].discard(victim_name)
        discovered[victim_target].add(victim_name + "-misspelled")
        with self.assertRaisesRegex(RuntimeError, "scenario discovery failed"):
            MODULE.check_manifest(discovered)

    def test_manifest_accepts_complete_discovery(self):
        discovered = {}
        for target, name in MODULE.SCENARIOS:
            discovered.setdefault(target, set()).add(name)
        MODULE.check_manifest(discovered)

    def test_zero_selected_tests_fails_closed(self):
        with self.assertRaisesRegex(RuntimeError, "selected zero tests"):
            MODULE.assert_tests_selected(
                "test result: ok. 0 passed; 0 failed; 0 ignored\n", "empty-case"
            )

    def test_missing_summary_fails_closed(self):
        with self.assertRaisesRegex(RuntimeError, "selected zero tests"):
            MODULE.assert_tests_selected(" Compiling nothing\n", "mystery-case")

    def test_executed_tests_pass_selection(self):
        MODULE.assert_tests_selected(
            "test result: ok. 5 passed; 0 failed; 0 ignored\n", "full-case"
        )

    def test_rejects_non_positive_rounds(self):
        with mock.patch.object(sys, "argv", ["races", "--rounds", "0"]):
            with self.assertRaises(SystemExit):
                MODULE.main()

    def test_rejects_non_positive_timeout(self):
        with mock.patch.object(sys, "argv", ["races", "--timeout", "0"]):
            with self.assertRaises(SystemExit):
                MODULE.main()

    def test_positive_arguments_reach_database_validation(self):
        out, err = io.StringIO(), io.StringIO()
        with mock.patch.object(
            sys, "argv", ["races", "--rounds", "1", "--timeout", "1"]
        ):
            with mock.patch.dict(os.environ, {}, clear=True):
                with redirect_stdout(out), redirect_stderr(err):
                    self.assertEqual(MODULE.main(), 1)
        self.assertIn("SOTTO_RUN_DB_TESTS", err.getvalue())


@unittest.skipIf(os.name == "nt", "process-group termination is POSIX-only")
class ProcessTreeTimeoutTests(unittest.TestCase):
    """Timeout ownership without Cargo, a database or sleep-based races."""

    def run_sleeper(self, script, timeout_seconds=5, label="tree-test"):
        with tempfile.TemporaryDirectory() as tmp:
            pid_file = os.path.join(tmp, "tree.pid")
            try:
                with self.assertRaises(MODULE.CommandFailed) as outcome:
                    MODULE.run(
                        [sys.executable, "-c", script, pid_file],
                        dict(os.environ),
                        timeout_seconds,
                        label=label,
                    )
                message = str(outcome.exception)
                self.assertIn(label, message)
                self.assertIn(f"timed out after {timeout_seconds}s", message)
                pids = read_pids(pid_file)
                self.assertEqual(len(pids), 2)
                for pid in pids:
                    self.assertFalse(
                        process_exists(pid), f"process {pid} survived the timeout"
                    )
            finally:
                kill_best_effort(pid_file)

    def test_timeout_terminates_child_and_grandchild(self):
        self.run_sleeper(SPAWNING_SLEEPER)

    def test_timeout_escalates_past_sigterm_ignore(self):
        self.run_sleeper(TERM_IGNORING_SLEEPER, label="escalation-test")

    def test_timeout_prints_partial_text_and_byte_output(self):
        out, err = io.StringIO(), io.StringIO()
        with tempfile.TemporaryDirectory() as tmp:
            pid_file = os.path.join(tmp, "tree.pid")
            try:
                with redirect_stdout(out), redirect_stderr(err):
                    with self.assertRaises(MODULE.CommandFailed):
                        MODULE.run(
                            [sys.executable, "-c", SPAWNING_SLEEPER, pid_file],
                            dict(os.environ),
                            5,
                            label="partial-test",
                        )
            finally:
                kill_best_effort(pid_file)
        self.assertIn("partial-test", out.getvalue())
        self.assertIn("partial-stdout", out.getvalue())
        self.assertIn("\ufffd", out.getvalue())
        self.assertIn("partial-stderr", err.getvalue())

    def test_timeout_embeds_partial_output_in_error(self):
        with tempfile.TemporaryDirectory() as tmp:
            pid_file = os.path.join(tmp, "tree.pid")
            try:
                with self.assertRaises(MODULE.CommandFailed) as outcome:
                    MODULE.run(
                        [sys.executable, "-c", SPAWNING_SLEEPER, pid_file],
                        dict(os.environ),
                        5,
                        label="embed-test",
                    )
            finally:
                kill_best_effort(pid_file)
        message = str(outcome.exception)
        self.assertIn("=== embed-test ===", message)
        self.assertIn("timed out after 5s", message)
        self.assertIn("partial-stdout", message)
        self.assertIn("partial-stderr", message)


class CommandResultTests(unittest.TestCase):
    def test_failure_renders_exactly_once(self):
        error = MODULE.CommandFailed(
            "exact-test", ["cargo", "test"], 3, False, 30, "out-part", "err-part"
        )
        self.assertEqual(
            str(error),
            "=== exact-test ===\n"
            "command: cargo test\n"
            "exit: 3\n"
            "--- stdout ---\n"
            "out-part\n"
            "--- stderr ---\n"
            "err-part",
        )
        self.assertEqual(error.command, ["cargo", "test"])

    def test_nonzero_exit_carries_label_and_both_streams(self):
        script = (
            "import sys; print('out-word'); "
            "print('err-word', file=sys.stderr); sys.exit(3)"
        )
        out, err = io.StringIO(), io.StringIO()
        with redirect_stdout(out), redirect_stderr(err):
            with self.assertRaises(MODULE.CommandFailed) as outcome:
                MODULE.run(
                    [sys.executable, "-c", script],
                    dict(os.environ),
                    30,
                    label="exit-test",
                )
        message = str(outcome.exception)
        self.assertIn("=== exit-test ===", message)
        self.assertIn("exit: 3", message)
        self.assertIn("out-word", message)
        self.assertIn("err-word", message)
        self.assertNotIn("out-word", out.getvalue().splitlines())
        self.assertNotIn("err-word", err.getvalue().splitlines())

    def test_success_returns_captured_output(self):
        out, err = io.StringIO(), io.StringIO()
        with redirect_stdout(out), redirect_stderr(err):
            result = MODULE.run(
                [sys.executable, "-c", "print('ok-word')"],
                dict(os.environ),
                30,
                label="success-test",
            )
        self.assertEqual(result.returncode, 0)
        self.assertIn("ok-word", result.stdout)
        self.assertIn("ok-word", out.getvalue())

    def test_parallel_failures_report_every_labelled_diagnostic(self):
        def failing(marker, code):
            return [
                sys.executable,
                "-c",
                f"import sys; print('{marker}-out'); "
                f"print('{marker}-err', file=sys.stderr); sys.exit({code})",
            ]

        out, err = io.StringIO(), io.StringIO()
        with redirect_stdout(out), redirect_stderr(err):
            with self.assertRaises(RuntimeError) as outcome:
                MODULE.run_parallel(
                    [
                        ("first-target", failing("first", 3)),
                        ("second-target", failing("second", 4)),
                    ],
                    dict(os.environ),
                    30,
                )
        message = str(outcome.exception)
        self.assertIn("parallel target failures (2)", message)
        for expected in (
            "=== first-target ===",
            "=== second-target ===",
            "first-out",
            "first-err",
            "second-out",
            "second-err",
            "exit: 3",
            "exit: 4",
        ):
            self.assertIn(expected, message)


if __name__ == "__main__":
    unittest.main()
