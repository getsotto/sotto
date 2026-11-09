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
LOADER = importlib.machinery.SourceFileLoader(
    "audit_policy", str(ROOT / "scripts/check-cargo-audit")
)
SPEC = importlib.util.spec_from_loader(LOADER.name, LOADER)
CHECKER = importlib.util.module_from_spec(SPEC)
LOADER.exec_module(CHECKER)
SOURCE = "registry+https://github.com/rust-lang/crates.io-index"
RSA = {"name": "rsa", "version": "0.9.10", "source": SOURCE}
SPIN = {"name": "spin", "version": "0.9.8", "source": SOURCE}
UNMAINTAINED = [
    {"name": "zbus_names", "version": "3.0.0", "source": SOURCE,
     "advisory": "RUSTSEC-2026-0371"},
    {"name": "zvariant", "version": "4.2.0", "source": SOURCE,
     "advisory": "RUSTSEC-2026-0370"},
    {"name": "zvariant_derive", "version": "4.2.0", "source": SOURCE,
     "advisory": "RUSTSEC-2026-0372"},
]


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
        "warnings": {
            "yanked": [{"kind": "yanked", "package": SPIN, "advisory": None}],
            "unmaintained": [
                {"kind": "unmaintained",
                 "package": {key: value for key, value in package.items() if key != "advisory"},
                 "advisory": {"id": package["advisory"]}}
                for package in UNMAINTAINED
            ],
        },
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
            f'source = "{SOURCE}"\n' for p in (RSA, SPIN, *[
                {key: value for key, value in package.items() if key != "advisory"}
                for package in UNMAINTAINED
            ])
        ))
        self.audit_report = report()
        self.audit_status = 1
        self.audit_stderr = ""
        self.audit_version = "cargo-audit-audit 0.22.2\n"
        self.registry_diagnostic = ""
        self.mutate_lock = False
        self.terminal_findings = (
            "Crate:     rsa\nVersion:   0.9.10\nID:        RUSTSEC-2023-0071\n\n"
            "Crate:     spin\nVersion:   0.9.8\nWarning:   yanked\n\n"
            "Crate:     zbus_names\nVersion:   3.0.0\nWarning:   unmaintained\n"
            "ID:        RUSTSEC-2026-0371\n\n"
            "Crate:     zvariant\nVersion:   4.2.0\nWarning:   unmaintained\n"
            "ID:        RUSTSEC-2026-0370\n\n"
            "Crate:     zvariant_derive\nVersion:   4.2.0\nWarning:   unmaintained\n"
            "ID:        RUSTSEC-2026-0372\n"
        )
        self.tree_output = "app v1.0.0\n\nanother-workspace-root v1.0.0\n"
        self.tree_status = 0
        self.calls = []

    def command(self, args, **kwargs):
        self.calls.append(args)
        if args[1] == "audit":
            if "--version" in args:
                return subprocess.CompletedProcess(args, 0, self.audit_version, "")
            if "--format" in args:
                output = (
                    "    Updating crates.io index\n"
                    "    Scanning Cargo.lock for vulnerabilities (2 crate dependencies)\n"
                )
                if self.audit_status == 1:
                    output += "error: 1 vulnerability found!\nerror: 4 denied warnings found!\n"
                    output += self.terminal_findings
                if self.mutate_lock:
                    self.lockfile.write_text(self.lockfile.read_text() + "\n")
                return subprocess.CompletedProcess(
                    args, self.audit_status, output, self.registry_diagnostic
                )
            return subprocess.CompletedProcess(
                args, self.audit_status, json.dumps(self.audit_report), self.audit_stderr
            )
        return subprocess.CompletedProcess(
            args, self.tree_status, self.tree_output, "tree diagnostic"
        )

    def check(self):
        with (
            patch.object(CHECKER.subprocess, "run", side_effect=self.command),
            contextlib.redirect_stdout(io.StringIO()),
        ):
            return CHECKER.check(self.root)

    def test_current_dormant_findings_pass_despite_audit_exit_one(self):
        self.check()
        trees = [args for args in self.calls if args[1] == "tree"]
        self.assertEqual(len(trees), 2)
        for args in trees:
            self.assertIn("--locked", args)
            self.assertIn("--workspace", args)
            self.assertEqual(args[args.index("--target") + 1], "all")
            self.assertEqual(args[args.index("--edges") + 1], "normal,build,dev")
        self.assertEqual(sum("--all-features" in args for args in trees), 1)

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
                with self.assertRaisesRegex(CHECKER.PolicyError, "unapproved"):
                    self.check()

    def test_new_yanked_and_informational_warnings_fail(self):
        for kind in ("yanked", "unmaintained", "unsound", "notice", "future-warning"):
            with self.subTest(kind=kind):
                self.audit_report = report()
                self.audit_report["warnings"].setdefault(kind, []).append({
                    "kind": kind,
                    "package": {"name": "other", "version": "1.0.0", "source": SOURCE},
                    "advisory": None if kind == "yanked" else {"id": "RUSTSEC-2026-9999"},
                })
                with self.assertRaisesRegex(CHECKER.PolicyError, "unapproved"):
                    self.check()

    def test_reachable_package_fails_with_dependency_path(self):
        self.tree_output = "sotto-server v0.6.0\nsqlx-mysql v0.8.6\nrsa v0.9.10\n"
        with self.assertRaisesRegex(CHECKER.PolicyError, "reachable.*\\n.*rsa"):
            self.check()

    def test_tool_failure_is_not_evidence_of_dormancy(self):
        for failing_tool in ("audit", "tree"):
            with self.subTest(tool=failing_tool):
                self.audit_status = 2 if failing_tool == "audit" else 1
                self.tree_status = 101 if failing_tool == "tree" else 0
                with self.assertRaisesRegex(CHECKER.PolicyError, f"cargo {failing_tool} failed"):
                    self.check()

    def test_empty_or_malformed_graph_is_not_evidence_of_dormancy(self):
        for output in ("", "\n\n", "unexpected cargo output\n"):
            with self.subTest(output=output):
                self.tree_output = output
                with self.assertRaisesRegex(CHECKER.PolicyError, "cargo tree"):
                    self.check()

    def test_audit_report_must_be_complete_and_consistent(self):
        cases = [None, {}, {"error": "network unavailable"}]
        for field, value in (("count", 0), ("count", True), ("found", False), ("list", {})):
            malformed = report()
            malformed["vulnerabilities"][field] = value
            cases.append(malformed)
        for malformed in cases:
            with self.subTest(report=malformed):
                self.audit_report = malformed
                with self.assertRaisesRegex(CHECKER.PolicyError, "report"):
                    self.check()

    def test_duplicate_json_keys_and_nonfinite_numbers_fail(self):
        valid = json.dumps(report())
        for invalid in (
            '{"warnings": {"yanked": [{"package": "hidden"}]},' + valid[1:],
            valid.replace('"advisory-count": 1239', '"advisory-count": NaN'),
            valid.replace('"advisory-count": 1239', '"advisory-count": true'),
            valid.replace('"dependency-count": 2', '"dependency-count": Infinity'),
        ):
            with self.subTest(invalid=invalid):
                with self.assertRaisesRegex(CHECKER.PolicyError, "report"):
                    CHECKER.parse_report(subprocess.CompletedProcess([], 1, invalid, ""))

    def test_audit_status_must_agree_with_report(self):
        self.audit_status = 0
        with self.assertRaisesRegex(CHECKER.PolicyError, "exit status"):
            self.check()

    def test_partial_yanked_lookup_failure_rejects_otherwise_valid_report(self):
        self.audit_stderr = (
            "error: could not check if other was yanked: failed to fetch registry entry\n"
        )
        with self.assertRaisesRegex(CHECKER.PolicyError, "diagnostics"):
            self.check()

    def test_registry_initialisation_failure_rejects_clean_json_scan(self):
        self.policy.write_text("version = 1\nallowed_dormant = []\n")
        self.audit_report["vulnerabilities"] = {"found": False, "count": 0, "list": []}
        self.audit_report["warnings"] = {}
        self.audit_status = 0
        self.registry_diagnostic = "warning: couldn't update crates.io index: network unavailable\n"
        with self.assertRaisesRegex(CHECKER.PolicyError, "registry diagnostics"):
            self.check()

    def test_recovered_lock_wait_is_allowed_only_with_a_complete_scan(self):
        warning = (
            "warning: directory /tmp/advisory-db is locked, waiting for up to 300 seconds "
            "for it to become available\n"
        )
        self.audit_stderr = warning
        self.registry_diagnostic = warning
        self.check()
        self.audit_stderr += "error: couldn't check if the package is yanked: network error\n"
        with self.assertRaisesRegex(CHECKER.PolicyError, "diagnostics"):
            self.check()

    def test_tool_version_mismatch_fails_before_auditing(self):
        for version in ("cargo-audit-audit 0.22.3", "cargo-audit-audit 0.21.2", ""):
            with self.subTest(version=version):
                self.audit_version = version
                self.calls.clear()
                with self.assertRaisesRegex(CHECKER.PolicyError, "0.22.2 required"):
                    self.check()
                self.assertFalse(any("--json" in args for args in self.calls))
                self.assertFalse(any(args[1] == "tree" for args in self.calls))

    def test_terminal_identities_must_match_even_when_counts_agree(self):
        original = self.terminal_findings
        for old, new in (("rsa", "other"), ("0.9.10", "0.9.11"),
                         ("RUSTSEC-2023-0071", "RUSTSEC-2026-9999"),
                         ("yanked", "unmaintained")):
            with self.subTest(old=old):
                self.terminal_findings = original.replace(old, new)
                with self.assertRaisesRegex(CHECKER.PolicyError, "identities differ"):
                    self.check()

    def test_incomplete_or_duplicate_terminal_findings_fail(self):
        original = self.terminal_findings
        for findings in (
            original + "Crate: ",
            original.replace("Version:   0.9.10\n", ""),
            original + original,
            original.replace("ID:        RUSTSEC-2023-0071\n", ""),
        ):
            with self.subTest(findings=findings):
                self.terminal_findings = findings
                with self.assertRaisesRegex(CHECKER.PolicyError, "terminal finding"):
                    self.check()

    def test_lockfile_change_between_scans_fails(self):
        self.mutate_lock = True
        with self.assertRaisesRegex(CHECKER.PolicyError, "lockfile changed during auditing"):
            self.check()

    def test_silent_json_registry_failure_needs_independent_terminal_success(self):
        # Model structurally valid JSON with no yanks and no stderr. It cannot
        # tell us whether its registry scan ran, so only the terminal scan can
        # establish a successful independent check after removing all exceptions.
        self.policy.write_text("version = 1\nallowed_dormant = []\n")
        self.audit_report["vulnerabilities"] = {"found": False, "count": 0, "list": []}
        self.audit_report["warnings"] = {}
        self.audit_status = 0
        self.registry_diagnostic = (
            "warning: directory /tmp/index is locked, waiting for up to 300 seconds "
            "for it to become available\n"
            "warning: couldn't update crates.io index: failed to obtain lock file\n"
        )
        with self.assertRaisesRegex(CHECKER.PolicyError, "registry diagnostics"):
            self.check()
        # If the transient lock clears, the complete independent scan is what
        # permits acceptance. This does not certify the earlier JSON scan.
        self.registry_diagnostic = ""
        self.check()

    def test_missing_finding_requires_exception_removal(self):
        self.audit_report["warnings"] = {}
        with self.assertRaisesRegex(CHECKER.PolicyError, "stale"):
            self.check()

    def test_empty_policy_passes_only_with_clean_audit(self):
        self.policy.write_text("version = 1\nallowed_dormant = []\n")
        with self.assertRaisesRegex(CHECKER.PolicyError, "unapproved"):
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
                with self.assertRaisesRegex(CHECKER.PolicyError, "policy"):
                    self.check()

    def test_changed_or_additional_lock_version_requires_policy_review(self):
        original = self.lockfile.read_text()
        for lock in (
            original.replace('version = "0.9.10"', 'version = "0.9.11"'),
            original + f'[[package]]\nname = "rsa"\nversion = "0.10.0"\nsource = "{SOURCE}"\n',
        ):
            with self.subTest(lock=lock):
                self.lockfile.write_text(lock)
                with self.assertRaisesRegex(CHECKER.PolicyError, "lockfile"):
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
                with self.assertRaisesRegex(CHECKER.PolicyError, "filtered"):
                    self.check()
        for field in ("database", "settings", "vulnerabilities", "warnings"):
            with self.subTest(missing=field):
                self.audit_report = report()
                del self.audit_report[field]
                with self.assertRaisesRegex(CHECKER.PolicyError, "report"):
                    self.check()


class InvocationTest(unittest.TestCase):
    def test_arguments_fail_with_documented_exit_code_and_usage(self):
        result = subprocess.run(
            [str(ROOT / "scripts/check-cargo-audit"), "unexpected"],
            cwd="/", capture_output=True, text=True, timeout=30,
        )
        self.assertEqual(result.returncode, 1)
        self.assertIn("usage: scripts/check-cargo-audit", result.stderr)


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
        self.app.write_text(
            self.app.read_text() + '\n[dependencies]\nadapter = { path = "../adapter" }\n'
        )
        adapter = self.root / "adapter/Cargo.toml"
        adapter.write_text(
            adapter.read_text() + '\n[dependencies]\nrsa = { path = "../rsa", optional = true }\n'
        )

    def lock(self):
        subprocess.run(["cargo", "generate-lockfile", "--offline"], cwd=self.root,
                       capture_output=True, text=True, check=True, timeout=30)

    def test_transitive_optional_crate_is_dormant(self):
        self.lock()
        CHECKER.check_dormant(self.root, {"rsa@0.9.10"})

    def test_normal_build_dev_and_non_host_target_edges_are_rejected(self):
        original = self.app.read_text()
        for section in ("dependencies", "build-dependencies", "dev-dependencies",
                        "target.'cfg(target_arch = \"wasm32\")'.dependencies"):
            with self.subTest(section=section):
                # Normal dependencies extend the existing table; the others open a new one.
                header = "" if section == "dependencies" else f"\n[{section}]\n"
                self.app.write_text(original + header + 'rsa = { path = "../rsa" }\n')
                self.lock()
                with self.assertRaisesRegex(CHECKER.PolicyError, "reachable"):
                    CHECKER.check_dormant(self.root, {"rsa@0.9.10"})

    def test_workspace_optional_feature_is_rejected(self):
        self.app.write_text(self.app.read_text() + 'rsa = { path = "../rsa", optional = true }\n')
        self.lock()
        with self.assertRaisesRegex(CHECKER.PolicyError, "reachable \\(all features\\)"):
            CHECKER.check_dormant(self.root, {"rsa@0.9.10"})

    def test_outdated_lockfile_fails_instead_of_being_regenerated(self):
        self.lock()
        before = (self.root / "Cargo.lock").read_bytes()
        self.app.write_text(self.app.read_text() + 'rsa = { path = "../rsa" }\n')
        with self.assertRaisesRegex(CHECKER.PolicyError, "cargo tree failed"):
            CHECKER.check_dormant(self.root, {"rsa@0.9.10"})
        self.assertEqual((self.root / "Cargo.lock").read_bytes(), before)

    def test_empty_policy_still_rejects_outdated_lockfile(self):
        self.lock()
        (self.root / ".ci").mkdir()
        policy = self.root / ".ci/cargo-audit-policy.toml"
        policy.write_text("version = 1\nallowed_dormant = []\n")
        self.app.write_text(self.app.read_text() + 'rsa = { path = "../rsa" }\n')
        with self.assertRaisesRegex(CHECKER.PolicyError, "cargo tree failed"):
            CHECKER.check(self.root)


if __name__ == "__main__":
    unittest.main()
