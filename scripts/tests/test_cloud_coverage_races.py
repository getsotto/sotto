from contextlib import redirect_stderr, redirect_stdout
from importlib.machinery import SourceFileLoader
import io
import os
import signal
import sys
import tempfile
from pathlib import Path
import unittest


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


@unittest.skipIf(os.name == "nt", "process-group termination is POSIX-only")
class ProcessTreeTimeoutTests(unittest.TestCase):
    """Timeout ownership without Cargo, a database or sleep-based races."""

    def run_sleeper(self, script, timeout_seconds=5, label="tree-test"):
        with tempfile.TemporaryDirectory() as tmp:
            pid_file = os.path.join(tmp, "tree.pid")
            try:
                with self.assertRaisesRegex(RuntimeError, f"{label}: timed out"):
                    MODULE.run(
                        [sys.executable, "-c", script, pid_file],
                        dict(os.environ),
                        timeout_seconds,
                        label=label,
                    )
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
                    with self.assertRaisesRegex(RuntimeError, "partial-test: timed out"):
                        MODULE.run(
                            [sys.executable, "-c", SPAWNING_SLEEPER, pid_file],
                            dict(os.environ),
                            5,
                            label="partial-test",
                        )
            finally:
                kill_best_effort(pid_file)
        self.assertIn("partial-stdout", out.getvalue())
        self.assertIn("\ufffd", out.getvalue())
        self.assertIn("partial-stderr", err.getvalue())


class CommandResultTests(unittest.TestCase):
    def test_nonzero_exit_preserves_both_streams(self):
        script = (
            "import sys; print('out-word'); "
            "print('err-word', file=sys.stderr); sys.exit(3)"
        )
        out, err = io.StringIO(), io.StringIO()
        with redirect_stdout(out), redirect_stderr(err):
            with self.assertRaisesRegex(RuntimeError, "command exited 3"):
                MODULE.run([sys.executable, "-c", script], dict(os.environ), 30)
        self.assertIn("out-word", out.getvalue())
        self.assertIn("err-word", err.getvalue())


if __name__ == "__main__":
    unittest.main()
