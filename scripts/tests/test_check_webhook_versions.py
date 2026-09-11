"""The drift check between live Stripe webhook endpoints and the versions this server accepts.

Two properties here are worth more than the rest. The parser must fail loudly rather than return
an empty list, because an empty list is indistinguishable from agreement in one direction and from
total drift in the other; and no test may let a webhook path reach the output, because this runs
in a public repository where the log and the issue it raises are both world-readable.
"""

import contextlib
import importlib.machinery
import importlib.util
import io
import json
import pathlib
import sys
import tempfile
import unittest
import unittest.mock
import urllib.error

ROOT = pathlib.Path(__file__).resolve().parents[2]
LOADER = importlib.machinery.SourceFileLoader(
    "check_webhook_versions", str(ROOT / "scripts/check-webhook-versions")
)
SPEC = importlib.util.spec_from_loader(LOADER.name, LOADER)
check = importlib.util.module_from_spec(SPEC)
sys.modules[LOADER.name] = check
LOADER.exec_module(check)

ACCEPTED = {"deployed": ["2026-06-24.dahlia", "2026-08-26.dahlia"]}


def endpoint(**overrides):
    base = {
        "id": "we_test",
        "url": "https://sotto.example/api/billing/stripe/webhook",
        "status": "enabled",
        "api_version": "2026-06-24.dahlia",
        "enabled_events": ["checkout.session.completed"],
    }
    base.update(overrides)
    return base


class AcceptedVersions(unittest.TestCase):
    def test_reads_the_constant_from_the_real_billing_source(self):
        # Against the actual file, not a fixture. A parser tested only on a handcrafted string
        # proves the string, and the shape that matters is the one in the repository.
        source = (ROOT / "crates/server/src/billing.rs").read_text(encoding="utf-8")
        versions = check.accepted_versions(source)
        self.assertIn("2026-06-24.dahlia", versions)
        self.assertTrue(all(v.count("-") == 2 for v in versions), versions)

    def test_a_missing_constant_raises_rather_than_returning_nothing(self):
        with self.assertRaises(ValueError):
            check.accepted_versions("pub const SOMETHING_ELSE: &[&str] = &[\"x\"];")

    def test_an_empty_constant_raises(self):
        # The dangerous shape: syntactically present, semantically nothing.
        with self.assertRaises(ValueError):
            check.accepted_versions(
                "pub const ACCEPTED_WEBHOOK_API_VERSIONS: &[&str] = &[];"
            )

    def test_a_neighbouring_constant_is_not_mistaken_for_it(self):
        source = (
            'pub const OTHER_VERSIONS: &[&str] = &["wrong.one"];\n'
            'pub const ACCEPTED_WEBHOOK_API_VERSIONS: &[&str] = &["right.one"];\n'
        )
        self.assertEqual(check.accepted_versions(source), ["right.one"])


class Redaction(unittest.TestCase):
    def test_the_path_never_survives(self):
        out = check.redact("https://sotto.example/api/billing/stripe/webhook?secret=1")
        self.assertNotIn("webhook", out)
        self.assertNotIn("secret", out)
        self.assertEqual(out, "https://sotto.example/…")

    def test_a_missing_or_unparseable_url_does_not_explode(self):
        self.assertEqual(check.redact(None), "(no url)")
        self.assertEqual(check.redact("not a url"), "(unparseable url)")


class Findings(unittest.TestCase):
    def test_agreement_reports_nothing(self):
        self.assertEqual(check.findings([endpoint()], ACCEPTED), [])

    def test_a_version_outside_the_list_is_named_with_the_source_that_refuses_it(self):
        out = check.findings([endpoint(api_version="2027-01-01.elder")], ACCEPTED)
        self.assertEqual(len(out), 1)
        self.assertIn("2027-01-01.elder", out[0])
        self.assertIn("deployed", out[0])

    def test_a_finding_carries_no_webhook_path(self):
        # The property the whole redaction exists for, asserted where it is actually emitted
        # rather than only on the helper.
        out = check.findings([endpoint(api_version="2027-01-01.elder")], ACCEPTED)
        self.assertNotIn("/api/billing", out[0])

    def test_an_unpinned_endpoint_is_a_finding_even_though_nothing_is_wrong_yet(self):
        out = check.findings([endpoint(api_version=None)], ACCEPTED)
        self.assertEqual(len(out), 1)
        self.assertIn("no pinned API version", out[0])

    def test_a_disabled_endpoint_is_ignored(self):
        drifted = endpoint(status="disabled", api_version="2027-01-01.elder")
        self.assertEqual(check.findings([drifted, endpoint()], ACCEPTED), [])

    def test_an_endpoint_for_unrelated_events_is_ignored(self):
        unrelated = endpoint(
            api_version="2027-01-01.elder", enabled_events=["payout.paid"]
        )
        self.assertEqual(check.findings([unrelated, endpoint()], ACCEPTED), [])

    def test_a_wildcard_subscription_counts_as_relevant(self):
        wild = endpoint(api_version="2027-01-01.elder", enabled_events=["*"])
        self.assertEqual(len(check.findings([wild], ACCEPTED)), 1)

    def test_no_relevant_endpoint_at_all_is_itself_the_alarm(self):
        # Deleting the endpoint breaks billing exactly as thoroughly as drifting it, and reports
        # as an empty list rather than as an error, which is why it is checked explicitly.
        out = check.findings([endpoint(enabled_events=["payout.paid"])], ACCEPTED)
        self.assertEqual(len(out), 1)
        self.assertIn("reach nothing", out[0])

    def test_a_version_accepted_by_one_source_and_not_another_names_only_the_refuser(self):
        both = {"deployed": ["2026-06-24.dahlia"], "main": ["2026-08-26.dahlia"]}
        out = check.findings([endpoint(api_version="2026-06-24.dahlia")], both)
        self.assertEqual(len(out), 1)
        self.assertIn("main", out[0])
        self.assertNotIn("deployed", out[0])


class UnrecognisedShape(unittest.TestCase):
    """What happens when a real endpoint record does not look like the fixtures.

    This is the acknowledged weak point: the code reads `status` and `enabled_events`, and if
    Stripe ever renames or omits either, every endpoint is filtered out. The question that matters
    is which way that fails. Filtering everything out reaches the "no relevant endpoint" branch and
    alarms, rather than reaching the end with nothing to complain about and reporting agreement.
    A false alarm costs somebody a look at the dashboard; a false pass costs twelve days.
    """

    def test_an_endpoint_with_no_status_field_alarms_rather_than_passing(self):
        stripped = endpoint()
        del stripped["status"]
        self.assertNotEqual(check.findings([stripped], ACCEPTED), [])

    def test_an_endpoint_with_no_events_field_alarms_rather_than_passing(self):
        stripped = endpoint()
        del stripped["enabled_events"]
        self.assertNotEqual(check.findings([stripped], ACCEPTED), [])

    def test_a_record_this_script_understands_nothing_about_alarms(self):
        self.assertNotEqual(check.findings([{"id": "we_odd"}], ACCEPTED), [])

    def test_an_empty_listing_alarms(self):
        # Not the same as "no endpoints drifted". Stripe returning nothing means billing webhooks
        # reach nothing at all, which is the outage rather than the absence of one.
        self.assertNotEqual(check.findings([], ACCEPTED), [])


class Fetch(unittest.TestCase):
    def test_the_key_travels_in_a_header_and_never_in_the_url(self):
        seen = {}

        def opener(request, timeout=None):
            seen["url"] = request.full_url
            seen["auth"] = request.get_header("Authorization")
            seen["timeout"] = timeout
            return io.StringIO(json.dumps({"data": [endpoint()], "has_more": False}))

        check.fetch_endpoints("rk_live_secret", opener=opener)
        self.assertNotIn("rk_live_secret", seen["url"])
        self.assertEqual(seen["auth"], "Bearer rk_live_secret")

    def test_the_request_is_bounded_by_a_timeout(self):
        # Unbounded, a stall holds the job until Actions kills it, and a killed job never reaches
        # the step that would say so. The hang and the pass look identical from outside.
        seen = {}

        def opener(request, timeout=None):
            seen["timeout"] = timeout
            return io.StringIO(json.dumps({"data": [], "has_more": False}))

        check.fetch_endpoints("rk_live_secret", opener=opener)
        self.assertEqual(seen["timeout"], check.TIMEOUT_SECONDS)
        self.assertTrue(0 < check.TIMEOUT_SECONDS <= 60)

    def test_a_truncated_listing_raises_rather_than_checking_a_subset(self):
        def opener(request, timeout=None):
            return io.StringIO(json.dumps({"data": [endpoint()], "has_more": True}))

        with self.assertRaises(ValueError):
            check.fetch_endpoints("rk_live_secret", opener=opener)


class ExitCodes(unittest.TestCase):
    """0 agrees, 1 has drifted, 2 could not tell. The workflow branches on the last two, and
    reporting an unreadable file or a dropped connection as drift is how an alert loses its
    meaning: the reader is sent to billing.rs over a problem that is not in it."""

    GOOD_SOURCE = 'pub const ACCEPTED_WEBHOOK_API_VERSIONS: &[&str] = &["2026-06-24.dahlia"];'

    def setUp(self):
        self.dir = tempfile.TemporaryDirectory()
        self.addCleanup(self.dir.cleanup)
        self.path = pathlib.Path(self.dir.name) / "billing.rs"
        self.path.write_text(self.GOOD_SOURCE, encoding="utf-8")
        self.args = ["--billing-source", f"live={self.path}"]
        patched = unittest.mock.patch.dict(
            "os.environ", {"STRIPE_LIVE_READ_KEY": "rk_live_fake"}
        )
        patched.start()
        self.addCleanup(patched.stop)

    def run_with(self, endpoints=None, raises=None):
        def fetch(key, opener=None):
            if raises is not None:
                raise raises
            return endpoints

        with unittest.mock.patch.object(check, "fetch_endpoints", fetch):
            return check.main(self.args)

    def test_agreement_is_zero(self):
        self.assertEqual(self.run_with([endpoint()]), 0)

    def test_drift_is_one(self):
        self.assertEqual(self.run_with([endpoint(api_version="2027-01-01.elder")]), 1)

    def test_a_missing_key_is_two(self):
        with unittest.mock.patch.dict("os.environ", {"STRIPE_LIVE_READ_KEY": ""}):
            self.assertEqual(self.run_with([endpoint()]), 2)

    def test_an_unreadable_source_is_two_not_drift(self):
        args = ["--billing-source", f"live={self.path.parent / 'absent.rs'}"]
        with unittest.mock.patch.object(check, "fetch_endpoints", lambda *a, **k: []):
            self.assertEqual(check.main(args), 2)

    def test_a_source_the_parser_cannot_read_is_two_not_drift(self):
        # The one that would bite in practice: someone renames or reshapes the constant, and a
        # check that answered 1 would send them hunting for a Stripe change that never happened.
        self.path.write_text("pub const SOMETHING_ELSE: &[&str] = &[];", encoding="utf-8")
        self.assertEqual(self.run_with([endpoint()]), 2)

    def test_a_truncated_listing_is_two_not_drift(self):
        self.assertEqual(self.run_with(raises=ValueError("too many endpoints")), 2)

    def test_a_dropped_connection_is_two_not_drift(self):
        self.assertEqual(self.run_with(raises=urllib.error.URLError("no route")), 2)

    def test_malformed_json_is_two_and_does_not_quote_the_body(self):
        broken = json.JSONDecodeError("Expecting value", "sk_live_leaked_in_a_body", 0)
        stderr = io.StringIO()
        with contextlib.redirect_stderr(stderr):
            self.assertEqual(self.run_with(raises=broken), 2)
        self.assertNotIn("sk_live_leaked_in_a_body", stderr.getvalue())


if __name__ == "__main__":
    unittest.main()
