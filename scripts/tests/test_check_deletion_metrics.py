"""The three stateless organisation-deletion alert rules, and the readings they are decided from.

Two properties here matter more than the rest. A deployment with no deletions emits none of the
per-state series at all, so absence has to read as healthy or this alarms for ever on an instance
where nothing is wrong; and a response that is not the exporter has to be told apart from one that
is, because both parse to nothing and only one of them means everything is fine.
"""

import http.client
import importlib.machinery
import importlib.util
import io
import pathlib
import sys
import unittest
import unittest.mock
import urllib.error
import urllib.request

ROOT = pathlib.Path(__file__).resolve().parents[2]
LOADER = importlib.machinery.SourceFileLoader(
    "check_deletion_metrics", str(ROOT / "scripts/check-deletion-metrics")
)
SPEC = importlib.util.spec_from_loader(LOADER.name, LOADER)
check = importlib.util.module_from_spec(SPEC)
sys.modules[LOADER.name] = check
LOADER.exec_module(check)

# What the exporter actually answers on this deployment today: the unconditional gauge and nothing
# else, because every other series comes from a GROUP BY over an empty table.
QUIET = "\n".join(
    [
        "# HELP sotto_organisation_deletion_purge_due_count Retention operations past deadline.",
        "# TYPE sotto_organisation_deletion_purge_due_count gauge",
        "sotto_organisation_deletion_purge_due_count 0",
    ]
)


def reading(**series):
    """Build a payload from `name_with_state=value` shorthand plus the unconditional gauge."""
    lines = [f"sotto_organisation_deletion_purge_due_count {series.pop('purge_due', 0)}"]
    for state, count in series.pop("operations", {}).items():
        lines.append(f'sotto_organisation_deletion_operations{{state="{state}"}} {count}')
    for state, age in series.pop("ages", {}).items():
        lines.append(f'sotto_organisation_deletion_oldest_age_seconds{{state="{state}"}} {age}')
    assert not series, series
    return check.parse("\n".join(lines))


class Parsing(unittest.TestCase):
    def test_a_bare_series_and_a_labelled_one(self):
        parsed = check.parse(QUIET)
        self.assertEqual(check.value(parsed, check.ALWAYS_PRESENT), 0.0)
        parsed = check.parse('sotto_organisation_deletion_operations{state="failed"} 3')
        self.assertEqual(
            check.value(parsed, check.OPERATIONS, state="failed"), 3.0
        )

    def test_comments_and_rubbish_lines_are_skipped_not_fatal(self):
        text = "\n".join(
            ["# HELP something", "", "   ", "not a sample at all", QUIET]
        )
        self.assertTrue(check.is_exporter_output(check.parse(text)))

    def test_an_escaped_label_value_is_unescaped(self):
        parsed = check.parse('some_metric{state="a\\"b"} 1')
        self.assertEqual(check.value(parsed, "some_metric", state='a"b'), 1.0)

    def test_a_value_that_is_not_a_finite_number_is_recorded_as_unreadable(self):
        # NaN is the dangerous one: every threshold compares with `>`, and `NaN > 0` is false, so
        # keeping it would let an unreadable reading answer "nothing is wrong". Dropping it
        # silently does the same by a longer route, because an absent series reads as zero. So it
        # is neither kept nor dropped: it is remembered as a thing that could not be read.
        for bad in ("NaN", "+Inf", "-Inf", "banana"):
            parsed = check.parse(f"sotto_organisation_deletion_purge_due_count {bad}")
            self.assertIsNone(check.value(parsed, check.ALWAYS_PRESENT), bad)
            self.assertEqual(check.unreadable_watched(parsed), ["the count of operations due for purge"], bad)


class UnreadableSeries(unittest.TestCase):
    """A series that is present and unreadable is neither absent nor healthy.

    On this exporter absence is health, so the two must not collapse into each other. The scenario
    below is the one that matters: a scrape that looks complete, answers every rule with silence,
    and reports that deletion is fine on the strength of a number nobody could read.
    """

    def test_an_unreadable_alerting_series_is_not_mistaken_for_an_absent_one(self):
        payload = "\n".join(
            [
                "sotto_organisation_deletion_purge_due_count 0",
                'sotto_organisation_deletion_operations{state="failed"} NaN',
            ]
        )
        parsed = check.parse(payload)
        self.assertTrue(check.is_exporter_output(parsed))
        self.assertEqual(check.findings(parsed), [], "the rules genuinely cannot see it")
        self.assertEqual(check.unreadable_watched(parsed), ["the count of failed operations"])

    def test_an_unreadable_series_no_rule_reads_is_ignored(self):
        # Rubbish in a metric nothing here looks at says nothing about deletion, and must not stop
        # the three rules that can still be answered.
        payload = "\n".join(
            [
                "sotto_organisation_deletion_purge_due_count 0",
                "sotto_organisation_deletion_purge_duration_maximum_seconds NaN",
                'sotto_organisation_deletion_attempts_total{metric="purge_attempts"} NaN',
            ]
        )
        self.assertEqual(check.unreadable_watched(check.parse(payload)), [])

    def test_an_unreadable_unconditional_gauge_is_still_the_exporter(self):
        # Otherwise this reports "that was not the exporter" for a response that plainly was, and
        # sends whoever reads it looking for a proxy problem instead of a broken counter.
        parsed = check.parse("sotto_organisation_deletion_purge_due_count NaN")
        self.assertTrue(check.is_exporter_output(parsed))

    def test_a_missing_series_reads_as_absent_rather_than_zero(self):
        self.assertIsNone(check.value(check.parse(QUIET), check.OPERATIONS, state="failed"))


class ExporterRecognition(unittest.TestCase):
    def test_the_quiet_exporter_is_recognised(self):
        self.assertTrue(check.is_exporter_output(check.parse(QUIET)))

    def test_things_that_are_not_the_exporter_are_not_mistaken_for_a_quiet_one(self):
        # All of these parse to nothing, exactly like a healthy deployment with no deletions. The
        # unconditional gauge is the only thing separating "asked and nothing to report" from
        # "asked something else entirely", and without it every one of these would read as green.
        for impostor in ("", "<!doctype html>", "not found", "# TYPE only_a_comment gauge"):
            self.assertFalse(check.is_exporter_output(check.parse(impostor)), impostor)


class Rules(unittest.TestCase):
    def test_a_deployment_with_no_deletions_needs_no_attention(self):
        # The state this instance is actually in, and the regression that would make the whole
        # check useless: none of the per-state series exists, and that is health, not silence.
        self.assertEqual(check.findings(check.parse(QUIET)), [])

    def test_a_failed_operation_is_reported(self):
        found = check.findings(reading(operations={"failed": 2}))
        self.assertEqual(len(found), 1)
        self.assertIn("failed state", found[0])

    def test_other_states_are_not_mistaken_for_failure(self):
        quiet = reading(operations={"requested": 3, "retention": 5, "completed": 9})
        self.assertEqual(check.findings(quiet), [])

    def test_an_operation_due_for_purge_is_reported(self):
        found = check.findings(reading(purge_due=1))
        self.assertEqual(len(found), 1)
        self.assertIn("purge deadline", found[0])

    def test_a_stuck_billing_cancellation_is_reported(self):
        found = check.findings(reading(ages={"cancelling_billing": 90000}))
        self.assertEqual(len(found), 1)
        self.assertIn("still being charged", found[0])

    def test_the_billing_limit_is_a_threshold_not_a_floor(self):
        # Exactly at the limit is not yet stuck; a second past it is. Asserted because an off-by-
        # one in the wrong direction here fires on every cancellation that takes a day.
        limit = check.BILLING_STUCK_SECONDS
        self.assertEqual(check.findings(reading(ages={"cancelling_billing": limit})), [])
        self.assertEqual(len(check.findings(reading(ages={"cancelling_billing": limit + 1}))), 1)

    def test_a_slow_cancellation_in_another_state_is_not_the_billing_rule(self):
        self.assertEqual(check.findings(reading(ages={"retention": 9_000_000})), [])

    def test_every_rule_can_fire_at_once(self):
        found = check.findings(
            reading(
                operations={"failed": 1},
                purge_due=2,
                ages={"cancelling_billing": 90000},
            )
        )
        self.assertEqual(len(found), 3)

    def test_a_finding_carries_counts_and_ages_and_nothing_else(self):
        # These reach a public issue. The exporter has no identifiers to leak by design, so this
        # guards the habit rather than a known leak: nothing from the payload is echoed verbatim.
        found = check.findings(reading(operations={"failed": 1}, purge_due=1))
        for line in found:
            self.assertNotIn("sotto_organisation_deletion", line)
            self.assertNotIn("{", line)


class Fetching(unittest.TestCase):
    def test_the_token_travels_in_a_header_and_never_in_the_url(self):
        seen = {}

        def opener(request, timeout=None):
            seen["url"] = request.full_url
            seen["auth"] = request.get_header("Authorization")
            seen["timeout"] = timeout
            # Bytes, because that is what urlopen hands back. An earlier version of this stub
            # returned str and passed a `fetch` that could never have worked against the real
            # thing, which is the whole failure mode a stub is supposed to avoid.
            return io.BytesIO(QUIET.encode())

        body = check.fetch("https://example.test/ops/metrics", "tok_secret", opener=opener)
        self.assertEqual(seen["auth"], "Bearer tok_secret")
        self.assertNotIn("tok_secret", seen["url"])
        self.assertEqual(seen["timeout"], check.TIMEOUT_SECONDS)
        self.assertTrue(check.is_exporter_output(check.parse(body)))

    def test_the_request_is_bounded(self):
        self.assertTrue(0 < check.TIMEOUT_SECONDS <= 60)

    def test_a_cleartext_url_is_refused_before_anything_is_sent(self):
        def opener(*_args, **_kwargs):
            raise AssertionError("a bearer must not reach a cleartext hop")

        for url in ("http://example.test/ops/metrics", "ftp://example.test/x", "//example.test/x"):
            with self.assertRaises(ValueError, msg=url):
                check.fetch(url, "tok_secret", opener=opener)

    def test_a_redirect_is_refused_rather_than_followed(self):
        # urllib copies request headers onto a redirected request, so a 302 to another host
        # arrives with the bearer intact. Asserted against the stock handler this replaces, so the
        # test states the risk rather than restating the fix.
        stock = urllib.request.HTTPRedirectHandler()
        request = urllib.request.Request(
            "https://good.example/ops/metrics", headers={"Authorization": "Bearer tok_secret"}
        )
        carried = stock.redirect_request(request, None, 302, "Found", {}, "https://elsewhere/x")
        self.assertEqual(carried.get_header("Authorization"), "Bearer tok_secret")
        self.assertIsNone(
            check.NoRedirects().redirect_request(
                request, None, 302, "Found", {}, "https://elsewhere/x"
            )
        )

    def test_the_opener_fetch_builds_carries_the_refusal(self):
        # The half the test above missed. Asserting that NoRedirects works says nothing about
        # whether anything uses it: swapping the default opener back to `urlopen` left every other
        # test passing while the bearer followed redirects again.
        seen = {}

        class FakeOpener:
            def open(self, _request, timeout=None):
                return io.BytesIO(QUIET.encode())

        def build_opener(*handlers):
            seen["handlers"] = handlers
            return FakeOpener()

        with unittest.mock.patch.object(urllib.request, "build_opener", build_opener):
            check.fetch("https://example.test/ops/metrics", "tok_secret")
        self.assertTrue(
            any(handler is check.NoRedirects for handler in seen.get("handlers", ())),
            f"fetch built its opener without NoRedirects: {seen}",
        )


class ExitCodes(unittest.TestCase):
    """0 is quiet, 1 needs a person, 2 could not tell. The workflow branches on the last two, and
    reporting an unreachable endpoint as a deletion failure sends somebody to the recovery runbook
    over a network blip."""

    ARGS = ["--url", "https://example.test/ops/metrics"]

    def run_with(self, body=None, raises=None, token="tok"):
        def fetch(_url, _token, opener=None):
            if raises is not None:
                raise raises
            return body

        with unittest.mock.patch.dict("os.environ", {"DELETION_METRICS_TOKEN": token}):
            with unittest.mock.patch.object(check, "fetch", fetch):
                return check.main(self.ARGS)

    def test_quiet_is_zero(self):
        self.assertEqual(self.run_with(QUIET), 0)

    def test_something_needing_attention_is_one(self):
        payload = QUIET.replace("purge_due_count 0", "purge_due_count 4")
        self.assertEqual(self.run_with(payload), 1)

    def test_a_missing_token_is_two(self):
        self.assertEqual(self.run_with(QUIET, token=""), 2)

    def test_a_refusal_is_two_not_an_alert(self):
        refused = urllib.error.HTTPError("u", 503, "unconfigured", {}, None)
        self.assertEqual(self.run_with(raises=refused), 2)

    def test_an_unreachable_endpoint_is_two_not_an_alert(self):
        self.assertEqual(self.run_with(raises=urllib.error.URLError("no route")), 2)

    def test_a_truncated_response_is_two_not_an_alert(self):
        # Raised rather than provoked, because the exception type is the thing being tested.
        # IncompleteRead is an HTTPException and not an OSError, so an `except OSError` handler
        # lets it escape as a traceback: Python exits 1, and the workflow publishes a peer closing
        # a socket as somebody's organisation stuck mid-delete.
        self.assertEqual(self.run_with(raises=http.client.IncompleteRead(b"half")), 2)

    def test_the_caught_types_are_the_ones_a_network_actually_raises(self):
        for raised in (
            urllib.error.URLError("x"),
            http.client.IncompleteRead(b""),
            http.client.RemoteDisconnected("x"),
            TimeoutError(),
        ):
            self.assertIsInstance(raised, check.TRANSPORT_FAILURES, type(raised).__name__)

    def test_an_unreadable_alerting_series_is_two_not_zero(self):
        # The whole point of the distinction: this scrape looks complete and answers every rule
        # with silence, and without the check it exits 0 and says deletion is healthy.
        payload = "\n".join(
            [
                "sotto_organisation_deletion_purge_due_count 0",
                'sotto_organisation_deletion_operations{state="failed"} NaN',
            ]
        )
        self.assertEqual(self.run_with(payload), 2)

    def test_a_response_that_is_not_the_exporter_is_two_not_zero(self):
        # The failure this whole check would otherwise have: a proxy error page parses to nothing,
        # finds no problems, and reports that deletion is healthy.
        self.assertEqual(self.run_with("<!doctype html><title>nope</title>"), 2)


if __name__ == "__main__":
    unittest.main()
