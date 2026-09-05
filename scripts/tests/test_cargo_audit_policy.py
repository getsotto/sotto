"""Policy behaviour at the command boundary; external audit/graph results are fixtures."""

import contextlib
import importlib.machinery
import importlib.util
import io
import json
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch


ROOT = Path(__file__).resolve().parents[2]
LOADER = importlib.machinery.SourceFileLoader("audit_policy", str(ROOT / "scripts/check-cargo-audit"))
SPEC = importlib.util.spec_from_loader(LOADER.name, LOADER)
checker = importlib.util.module_from_spec(SPEC)
LOADER.exec_module(checker)
SOURCE = "registry+https://github.com/rust-lang/crates.io-index"
RSA = {"name": "rsa", "version": "0.9.10", "source": SOURCE}
SPIN = {"name": "spin", "version": "0.9.8", "source": SOURCE}


def report():
    return {
        "database": {"advisory-count": 1239, "last-commit": "a" * 40},
        "lockfile": {"dependency-count": 2},
        "settings": {
            "target_arch": [], "target_os": [], "severity": None, "ignore": [],
            "informational_warnings": ["unmaintained", "unsound", "notice"],
        },
        "vulnerabilities": {
            "found": True, "count": 1,
            "list": [{"package": RSA, "advisory": {"id": "RUSTSEC-2023-0071"}}],
        },
        "warnings": {"yanked": [{"kind": "yanked", "package": SPIN, "advisory": None}]},
    }


class PolicyTest(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        (self.root / ".ci").mkdir()
        self.policy = self.root / ".ci/cargo-audit-policy.toml"
        self.policy.write_text((ROOT / ".ci/cargo-audit-policy.toml").read_text())
        self.lockfile = self.root / "Cargo.lock"
        self.lockfile.write_text('version = 4\n' + ''.join(
            f'[[package]]\nname = "{p["name"]}"\nversion = "{p["version"]}"\n'
            f'source = "{SOURCE}"\n' for p in (RSA, SPIN)
        ))
        self.audit_report = report()
        self.audit_status = 1
        self.tree_output = "app v1.0.0\n\nanother-workspace-root v1.0.0\n"
        self.tree_status = 0
        self.calls = []

    def command(self, args, **kwargs):
        self.calls.append(args)
        if args[1] == "audit":
            return subprocess.CompletedProcess(args, self.audit_status, json.dumps(self.audit_report), "")
        return subprocess.CompletedProcess(args, self.tree_status, self.tree_output, "tree diagnostic")

    def check(self):
        with patch.object(checker.subprocess, "run", side_effect=self.command), contextlib.redirect_stdout(io.StringIO()):
            return checker.check(self.root)

    def test_current_dormant_findings_pass_despite_audit_exit_one(self):
        self.check()
        trees = [args for args in self.calls if args[1] == "tree"]
        self.assertEqual(len(trees), 4)
        for args in trees:
            self.assertIn("--locked", args)
            self.assertIn("--workspace", args)
            self.assertEqual(args[args.index("--target") + 1], "all")
            self.assertEqual(args[args.index("--edges") + 1], "normal,build,dev")
        self.assertEqual(sum("--all-features" in args for args in trees), 2)

    def test_new_findings_are_never_covered_by_package_exceptions(self):
        for name, version, source, advisory in (
            ("other", "1.0.0", SOURCE, "RUSTSEC-2026-9999"),
            ("rsa", "0.9.10", SOURCE, "RUSTSEC-2026-9999"),
            ("rsa", "0.9.11", SOURCE, "RUSTSEC-2023-0071"),
            ("rsa", "0.9.10", "registry+https://example.com/index", "RUSTSEC-2023-0071"),
        ):
            with self.subTest(name=name, version=version, source=source, advisory=advisory):
                self.audit_report = report()
                self.audit_report["vulnerabilities"]["list"].append({
                    "package": {"name": name, "version": version, "source": source},
                    "advisory": {"id": advisory},
                })
                self.audit_report["vulnerabilities"]["count"] += 1
                with self.assertRaisesRegex(checker.PolicyError, "unapproved"):
                    self.check()

    def test_new_yanked_and_informational_warnings_fail(self):
        for kind in ("yanked", "unmaintained", "unsound", "notice", "future-warning"):
            with self.subTest(kind=kind):
                self.audit_report = report()
                self.audit_report["warnings"].setdefault(kind, []).append({
                    "kind": kind, "package": {"name": "other", "version": "1.0.0", "source": SOURCE},
                    "advisory": None if kind == "yanked" else {"id": "RUSTSEC-2026-9999"},
                })
                with self.assertRaisesRegex(checker.PolicyError, "unapproved"):
                    self.check()

    def test_reachable_package_fails_with_dependency_path(self):
        self.tree_output = "sotto-server v0.6.0\nsqlx-mysql v0.8.6\nrsa v0.9.10\n"
        with self.assertRaisesRegex(checker.PolicyError, "reachable.*\\n.*rsa"):
            self.check()

    def test_tool_failure_is_not_evidence_of_dormancy(self):
        for failing_tool in ("audit", "tree"):
            with self.subTest(tool=failing_tool):
                self.audit_status = 2 if failing_tool == "audit" else 1
                self.tree_status = 101 if failing_tool == "tree" else 0
                with self.assertRaisesRegex(checker.PolicyError, f"cargo {failing_tool} failed"):
                    self.check()

    def test_empty_or_malformed_graph_is_not_evidence_of_dormancy(self):
        for output in ("", "\n\n", "unexpected cargo output\n"):
            with self.subTest(output=output):
                self.tree_output = output
                with self.assertRaisesRegex(checker.PolicyError, "cargo tree"):
                    self.check()

    def test_audit_report_must_be_complete_and_consistent(self):
        cases = [None, {}, {"error": "network unavailable"}]
        for field, value in (("count", 0), ("found", False), ("list", {})):
            malformed = report()
            malformed["vulnerabilities"][field] = value
            cases.append(malformed)
        for malformed in cases:
            with self.subTest(report=malformed):
                self.audit_report = malformed
                with self.assertRaisesRegex(checker.PolicyError, "report"):
                    self.check()

    def test_audit_status_must_agree_with_report(self):
        self.audit_status = 0
        with self.assertRaisesRegex(checker.PolicyError, "exit status"):
            self.check()

    def test_missing_finding_requires_exception_removal(self):
        self.audit_report["warnings"] = {}
        with self.assertRaisesRegex(checker.PolicyError, "stale"):
            self.check()

    def test_empty_policy_passes_only_with_clean_audit(self):
        self.policy.write_text("version = 1\nallowed_dormant = []\n")
        with self.assertRaisesRegex(checker.PolicyError, "unapproved"):
            self.check()
        self.audit_report["vulnerabilities"] = {"found": False, "count": 0, "list": []}
        self.audit_report["warnings"] = {}
        self.audit_status = 0
        self.check()

    def test_malformed_or_duplicate_policy_fails(self):
        original = self.policy.read_text()
        for policy in (
            original.replace("version = 1", "version = 2", 1),
            original.replace("reason =", "typo =", 1),
            original + original[original.index("[[allowed_dormant]]"):],
            original.replace('package = "rsa"', 'package = ""'),
            original.replace('version = "0.9.10"', 'version = "*"'),
        ):
            with self.subTest(policy=policy):
                self.policy.write_text(policy)
                with self.assertRaisesRegex(checker.PolicyError, "policy"):
                    self.check()

    def test_changed_or_additional_lock_version_requires_policy_review(self):
        original = self.lockfile.read_text()
        for lock in (
            original.replace('version = "0.9.10"', 'version = "0.9.11"'),
            original + f'[[package]]\nname = "rsa"\nversion = "0.10.0"\nsource = "{SOURCE}"\n',
        ):
            with self.subTest(lock=lock):
                self.lockfile.write_text(lock)
                with self.assertRaisesRegex(checker.PolicyError, "lockfile"):
                    self.check()

    def test_filtered_or_incomplete_report_fails(self):
        for field, value in (
            ("ignore", ["RUSTSEC-2026-9999"]), ("target_os", ["linux"]),
            ("target_arch", ["x86_64"]), ("severity", "high"),
            ("informational_warnings", []),
        ):
            with self.subTest(field=field):
                self.audit_report = report()
                self.audit_report["settings"][field] = value
                with self.assertRaisesRegex(checker.PolicyError, "filtered"):
                    self.check()
        for field in ("database", "settings", "vulnerabilities", "warnings"):
            with self.subTest(missing=field):
                self.audit_report = report()
                del self.audit_report[field]
                with self.assertRaisesRegex(checker.PolicyError, "report"):
                    self.check()


class CargoGraphTest(unittest.TestCase):
    """Real offline Cargo resolution, including dormant transitive optional crates."""

    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        (self.root / "Cargo.toml").write_text(
            '[workspace]\nresolver = "2"\nmembers = ["app"]\nexclude = ["adapter", "rsa"]\n'
        )
        for name, version in (("app", "1.0.0"), ("adapter", "1.0.0"), ("rsa", "0.9.10")):
            directory = self.root / name
            (directory / "src").mkdir(parents=True)
            (directory / "src/lib.rs").write_text("")
            (directory / "Cargo.toml").write_text(
                f'[package]\nname = "{name}"\nversion = "{version}"\nedition = "2021"\n'
            )
        self.app = self.root / "app/Cargo.toml"
        self.app.write_text(self.app.read_text() + '\n[dependencies]\nadapter = { path = "../adapter" }\n')
        adapter = self.root / "adapter/Cargo.toml"
        adapter.write_text(adapter.read_text() + '\n[dependencies]\nrsa = { path = "../rsa", optional = true }\n')

    def lock(self):
        subprocess.run(["cargo", "generate-lockfile", "--offline"], cwd=self.root,
                       capture_output=True, text=True, check=True, timeout=30)

    def test_transitive_optional_crate_is_dormant(self):
        self.lock()
        checker.check_dormant(self.root, "rsa@0.9.10")

    def test_normal_build_dev_and_non_host_target_edges_are_rejected(self):
        original = self.app.read_text()
        for section in ("dependencies", "build-dependencies", "dev-dependencies",
                        "target.'cfg(target_arch = \"wasm32\")'.dependencies"):
            with self.subTest(section=section):
                # Normal dependencies extend the existing table; the others open a new one.
                header = "" if section == "dependencies" else f"\n[{section}]\n"
                self.app.write_text(original + header + 'rsa = { path = "../rsa" }\n')
                self.lock()
                with self.assertRaisesRegex(checker.PolicyError, "reachable"):
                    checker.check_dormant(self.root, "rsa@0.9.10")

    def test_workspace_optional_feature_is_rejected(self):
        self.app.write_text(self.app.read_text() + 'rsa = { path = "../rsa", optional = true }\n')
        self.lock()
        with self.assertRaisesRegex(checker.PolicyError, "reachable \\(all features\\)"):
            checker.check_dormant(self.root, "rsa@0.9.10")

    def test_outdated_lockfile_fails_instead_of_being_regenerated(self):
        self.lock()
        before = (self.root / "Cargo.lock").read_bytes()
        self.app.write_text(self.app.read_text() + 'rsa = { path = "../rsa" }\n')
        with self.assertRaisesRegex(checker.PolicyError, "cargo tree failed"):
            checker.check_dormant(self.root, "rsa@0.9.10")
        self.assertEqual((self.root / "Cargo.lock").read_bytes(), before)


if __name__ == "__main__":
    unittest.main()
