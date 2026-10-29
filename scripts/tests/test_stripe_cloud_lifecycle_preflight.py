import importlib.util
import io
import os
from importlib.machinery import SourceFileLoader
from pathlib import Path
import re
import stat
import sys
import tempfile
import unittest
from contextlib import redirect_stderr, redirect_stdout
from unittest import mock

ROOT = Path(__file__).resolve().parents[2]
LOADER = SourceFileLoader(
    "stripe_cloud_lifecycle_preflight", str(ROOT / "scripts/stripe-cloud-lifecycle-preflight")
)
SPEC = importlib.util.spec_from_loader(LOADER.name, LOADER)
MODULE = importlib.util.module_from_spec(SPEC)
sys.modules[LOADER.name] = MODULE
LOADER.exec_module(MODULE)


def account(account_id="acct_123"):
    return {"id": account_id, "object": "account"}


def price(price_id, amount, interval, livemode=False, usage_type="licensed"):
    return {
        "id": price_id,
        "active": True,
        "livemode": livemode,
        "currency": "gbp",
        "unit_amount": amount,
        "recurring": {
            "interval": interval,
            "interval_count": 1,
            "usage_type": usage_type,
        },
    }


def prices():
    return {
        "standard_monthly": "price_standard_monthly",
        "standard_annual": "price_standard_annual",
        "founding_monthly": "price_founding_monthly",
        "founding_annual": "price_founding_annual",
    }


def provider_response(path, _api_key):
    if path == "/v1/account":
        return account()
    price_id = path.removeprefix("/v1/prices/")
    fixtures = {
        "price_standard_monthly": price(price_id, 299, "month"),
        "price_standard_annual": price(price_id, 2999, "year"),
        "price_founding_monthly": price(price_id, 199, "month"),
        "price_founding_annual": price(price_id, 1999, "year"),
    }
    return fixtures[price_id]


class SandboxPreflight(unittest.TestCase):
    def test_price_expectations_match_the_server_catalogue(self):
        source = (ROOT / "crates/server/src/billing_catalogue.rs").read_text()
        constants = {
            name: int(value.replace("_", ""))
            for name, value in re.findall(
                r"pub const (STANDARD_MONTHLY_PENCE|STANDARD_ANNUAL_PENCE|"
                r"FOUNDING_MONTHLY_PENCE|FOUNDING_ANNUAL_PENCE): i64 = ([0-9_]+);",
                source,
            )
        }
        expected = {
            "standard_monthly": constants["STANDARD_MONTHLY_PENCE"],
            "standard_annual": constants["STANDARD_ANNUAL_PENCE"],
            "founding_monthly": constants["FOUNDING_MONTHLY_PENCE"],
            "founding_annual": constants["FOUNDING_ANNUAL_PENCE"],
        }
        self.assertEqual({name: details[1] for name, details in MODULE.OFFERS.items()}, expected)

    def test_matching_test_account_and_catalogue_produce_only_fingerprints(self):
        report = MODULE.run_preflight(
            "acct_123", prices(), "rk_test_never_print_this", provider_response
        )

        self.assertTrue(report["preflight_passed"])
        self.assertFalse(report["scenarios_executed"])
        self.assertFalse(report["write_permissions_verified"])
        self.assertEqual(report["api_version"], MODULE.server_api_version())
        self.assertEqual(report["sandbox_account_fingerprint"], MODULE.fingerprint("acct_123"))
        self.assertEqual(len(report["prices"]), 4)
        encoded = str(report)
        self.assertNotIn("acct_123", encoded)
        self.assertNotIn("price_standard_monthly", encoded)
        self.assertNotIn("rk_test_never_print_this", encoded)

    def test_account_without_livemode_field_is_accepted(self):
        calls = []

        def response(path, key):
            calls.append(path)
            return provider_response(path, key)

        report = MODULE.run_preflight("acct_123", prices(), "rk_test_secret", response)
        self.assertTrue(report["preflight_passed"])
        self.assertEqual(calls[0], "/v1/account")
        self.assertEqual(len(calls), 5)

    def test_wrong_account_is_rejected_without_exposing_ids(self):
        with self.assertRaises(MODULE.PreflightError) as error:
            MODULE.run_preflight(
                "acct_123", prices(), "rk_test_secret",
                lambda _path, _key: account("acct_456"),
            )
        self.assertEqual(str(error.exception), "sandbox_account_identity_mismatch")
        self.assertNotIn("acct_", str(error.exception))

    def test_price_response_rejects_each_server_incompatible_field(self):
        cases = [
            ("id", lambda value: value.update(id="price_other")),
            ("active", lambda value: value.update(active=False)),
            ("livemode", lambda value: value.update(livemode=True)),
            ("currency", lambda value: value.update(currency="usd")),
            ("amount", lambda value: value.update(unit_amount=1)),
            ("fractional amount", lambda value: value.update(unit_amount=299.0)),
            ("interval", lambda value: value["recurring"].update(interval="year")),
            ("interval count", lambda value: value["recurring"].update(interval_count=2)),
            (
                "fractional interval count",
                lambda value: value["recurring"].update(interval_count=1.0),
            ),
            (
                "boolean interval count",
                lambda value: value["recurring"].update(interval_count=True),
            ),
            (
                "metered usage",
                lambda value: value["recurring"].update(usage_type="metered"),
            ),
            (
                "missing usage type",
                lambda value: value["recurring"].pop("usage_type"),
            ),
        ]

        for name, mutate in cases:
            with self.subTest(name=name):
                def response(path, key):
                    result = provider_response(path, key)
                    if path == "/v1/prices/price_standard_monthly":
                        mutate(result)
                    return result

                with self.assertRaisesRegex(
                    MODULE.PreflightError, "price_catalogue_mismatch:standard_monthly"
                ):
                    MODULE.run_preflight("acct_123", prices(), "rk_test_secret", response)

    def test_duplicate_price_ids_are_rejected_before_provider_reads(self):
        configured = prices()
        configured["founding_monthly"] = configured["standard_monthly"]
        calls = []
        with self.assertRaisesRegex(
            MODULE.PreflightError, "configured_price_ids_must_be_unique"
        ):
            MODULE.run_preflight(
                "acct_123", configured, "rk_test_secret", lambda *args: calls.append(args)
            )
        self.assertEqual(calls, [])

    def test_invalid_preflight_inputs_are_rejected_before_provider_reads(self):
        invalid_inputs = [
            ("account id", "not-an-account", prices(), "a" * 40),
            ("price set", "acct_123", {"standard_monthly": "price_one"}, "a" * 40),
            (
                "price id",
                "acct_123",
                {**prices(), "standard_monthly": "not-a-price"},
                "a" * 40,
            ),
            ("candidate sha", "acct_123", prices(), "not-a-full-sha"),
        ]
        for name, account_id, configured, app_sha in invalid_inputs:
            with self.subTest(name=name):
                calls = []
                with self.assertRaises(MODULE.PreflightError):
                    MODULE.run_preflight(
                        account_id,
                        configured,
                        "rk_test_secret",
                        lambda *args: calls.append(args),
                        app_sha=app_sha,
                    )
                self.assertEqual(calls, [])

    def test_non_test_key_is_rejected_before_provider_reads(self):
        calls = []
        with self.assertRaisesRegex(MODULE.PreflightError, "restricted_test_key_required"):
            MODULE.run_preflight(
                "acct_123", prices(), "sk_live_secret", lambda *args: calls.append(args)
            )
        self.assertEqual(calls, [])

    def test_provider_reads_use_the_fixed_host_and_server_api_version(self):
        class Response:
            def __enter__(self):
                return self

            def __exit__(self, *_args):
                return False

            def read(self, _limit):
                return b'{"id":"acct_123","object":"account"}'

        with mock.patch.object(MODULE, "build_opener") as build_opener:
            build_opener.return_value.open.return_value = Response()
            MODULE.stripe_json("/v1/account", "rk_test_secret")
        self.assertIsInstance(build_opener.call_args.args[0], MODULE.RejectStripeRedirects)
        request = build_opener.return_value.open.call_args.args[0]
        self.assertEqual(request.full_url, "https://api.stripe.com/v1/account")
        self.assertEqual(request.get_method(), "GET")
        self.assertEqual(request.get_header("Stripe-version"), MODULE.server_api_version())
        self.assertEqual(request.get_header("Authorization"), "Bearer rk_test_secret")

    def test_redirect_handler_rejects_other_hosts(self):
        request = MODULE.Request("https://api.stripe.com/v1/account")
        with self.assertRaises(MODULE.HTTPError) as raised:
            MODULE.RejectStripeRedirects().redirect_request(
                request, None, 302, "Found", {}, "https://attacker.example/v1/account"
            )
        self.assertEqual(raised.exception.code, 302)
        self.assertEqual(raised.exception.url, "https://api.stripe.com/v1/account")
        raised.exception.close()

    def test_http_error_diagnostic_does_not_include_provider_response(self):
        error = MODULE.HTTPError(
            "https://api.stripe.com/v1/account", 403, "restricted", {}, None
        )
        with mock.patch.object(MODULE, "build_opener") as build_opener:
            build_opener.return_value.open.side_effect = error
            with self.assertRaises(MODULE.PreflightError) as raised:
                MODULE.stripe_json("/v1/account", "rk_test_secret")
        self.assertEqual(str(raised.exception), "stripe_read_rejected")
        self.assertNotIn("rk_test_secret", str(raised.exception))

    def test_network_failure_diagnostic_does_not_include_provider_details(self):
        with mock.patch.object(MODULE, "build_opener") as build_opener:
            build_opener.return_value.open.side_effect = MODULE.URLError(
                "rk_test_secret must not escape"
            )
            with self.assertRaises(MODULE.PreflightError) as raised:
                MODULE.stripe_json("/v1/account", "rk_test_secret")
        self.assertEqual(str(raised.exception), "stripe_read_unavailable")
        self.assertNotIn("rk_test_secret", str(raised.exception))

    def test_invalid_and_oversized_provider_bodies_fail_closed(self):
        class Response:
            def __init__(self, raw):
                self.raw = raw

            def __enter__(self):
                return self

            def __exit__(self, *_args):
                return False

            def read(self, limit):
                return self.raw[:limit]

        invalid_bodies = [
            ("malformed json", b"not-json", "stripe_response_invalid"),
            ("invalid UTF-8", b"\xff", "stripe_response_invalid"),
            ("non-object JSON", b"[]", "stripe_response_invalid"),
            (
                "oversized body",
                b" " * (MODULE.MAX_RESPONSE_BYTES + 1),
                "stripe_response_too_large",
            ),
        ]
        for name, raw, error_code in invalid_bodies:
            with self.subTest(name=name):
                with mock.patch.object(MODULE, "build_opener") as build_opener:
                    build_opener.return_value.open.return_value = Response(raw)
                    with self.assertRaisesRegex(MODULE.PreflightError, error_code):
                        MODULE.stripe_json("/v1/account", "rk_test_secret")

    def test_key_file_must_be_private_owned_and_not_a_symlink(self):
        with tempfile.TemporaryDirectory() as directory:
            parent = Path(directory)
            parent.chmod(0o700)
            key_file = parent / "key"
            key_file.write_text("rk_test_private\n")
            key_file.chmod(0o600)
            self.assertEqual(MODULE.read_restricted_key(key_file), "rk_test_private")
            self.assertEqual(stat.S_IMODE(key_file.stat().st_mode), 0o600)

            key_file.chmod(0o644)
            with self.assertRaisesRegex(MODULE.PreflightError, "key_file_permissions_invalid"):
                MODULE.read_restricted_key(key_file)

            key_file.chmod(0o600)
            link = parent / "key-link"
            link.symlink_to(key_file)
            with self.assertRaisesRegex(MODULE.PreflightError, "key_file_path_invalid"):
                MODULE.read_restricted_key(link)

            parent.chmod(0o755)
            with self.assertRaisesRegex(
                MODULE.PreflightError, "key_file_permissions_invalid"
            ):
                MODULE.read_restricted_key(key_file)

    def test_private_report_refuses_repository_paths_and_writes_owner_only(self):
        report = {"preflight_passed": True}
        with tempfile.TemporaryDirectory() as directory:
            parent = Path(directory) / "private"
            parent.mkdir(mode=0o700)
            destination = parent / "preflight.json"
            MODULE.write_private_report(destination, report)
            self.assertEqual(stat.S_IMODE(destination.stat().st_mode), 0o600)
            self.assertEqual(MODULE.json.loads(destination.read_text()), report)
            link = parent / "report-link"
            link.symlink_to(destination)
            with self.assertRaisesRegex(MODULE.PreflightError, "report_path_invalid"):
                MODULE.write_private_report(link, report)

        with self.assertRaisesRegex(MODULE.PreflightError, "report_must_be_outside_repository"):
            MODULE.write_private_report(ROOT / "preflight.json", report)

    def test_private_report_rejects_relative_paths_and_permissive_directories(self):
        report = {"preflight_passed": True}
        with self.assertRaisesRegex(MODULE.PreflightError, "report_path_invalid"):
            MODULE.write_private_report("relative-report.json", report)

        with tempfile.TemporaryDirectory() as directory:
            parent = Path(directory)
            parent.chmod(0o755)
            with self.assertRaisesRegex(
                MODULE.PreflightError, "report_directory_permissions_invalid"
            ):
                MODULE.write_private_report(parent / "preflight.json", report)

    def test_cli_writes_a_private_fingerprinted_report_without_printing_the_key(self):
        with tempfile.TemporaryDirectory() as directory:
            parent = Path(directory)
            parent.chmod(0o700)
            key_file = parent / "test-key"
            key_file.write_text("rk_test_cli_secret\n")
            key_file.chmod(0o600)
            output = parent / "report.json"
            env = {
                "STRIPE_CLOUD_LIFECYCLE_KEY_FILE": str(key_file),
                "STRIPE_CLOUD_LIFECYCLE_ACCOUNT_ID": "acct_123",
                "STRIPE_STANDARD_MONTHLY_PRICE_ID": "price_standard_monthly",
                "STRIPE_STANDARD_ANNUAL_PRICE_ID": "price_standard_annual",
                "STRIPE_FOUNDING_MONTHLY_PRICE_ID": "price_founding_monthly",
                "STRIPE_FOUNDING_ANNUAL_PRICE_ID": "price_founding_annual",
                "SOTTO_CANDIDATE_SHA": "a" * 40,
            }
            with mock.patch.dict(os.environ, env, clear=True):
                with mock.patch.object(MODULE, "stripe_json", side_effect=provider_response):
                    captured = io.StringIO()
                    with redirect_stdout(captured):
                        self.assertEqual(MODULE.main(["--output", str(output)]), 0)

            self.assertEqual(stat.S_IMODE(output.stat().st_mode), 0o600)
            report = MODULE.json.loads(output.read_text())
            self.assertEqual(report["app_sha"], "a" * 40)
            self.assertTrue(report["preflight_passed"])
            self.assertNotIn("rk_test_cli_secret", captured.getvalue())
            self.assertNotIn("rk_test_cli_secret", output.read_text())

    def test_cli_stops_before_provider_reads_when_configuration_is_missing(self):
        with mock.patch.dict(os.environ, {}, clear=True):
            with mock.patch.object(MODULE, "stripe_json") as stripe_json:
                captured = io.StringIO()
                with redirect_stderr(captured):
                    self.assertEqual(MODULE.main(["--output", "/tmp/private/preflight.json"]), 1)
        self.assertIn("required_configuration_missing:STRIPE_CLOUD_LIFECYCLE_KEY_FILE", captured.getvalue())
        stripe_json.assert_not_called()


if __name__ == "__main__":
    unittest.main()
