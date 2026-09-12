"""Status probe verdicts and the running summary they accumulate into.

The verdicts are the part worth pinning down. Each one decides whether a public answer
means the component works, and several of the wrong readings are worse than a missed
outage: an unsigned webhook that succeeds is a security failure reported as green, and the
single-page app answering for the API is the exact shape of a misconfigured deployment
that looks healthy from outside.
"""

import contextlib
import datetime as dt
import http.client
import importlib.machinery
import importlib.util
import json
import sys
import tempfile
import unittest
import unittest.mock
import urllib.error
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
LOADER = importlib.machinery.SourceFileLoader("status_probe", str(ROOT / "scripts/status-probe"))
SPEC = importlib.util.spec_from_loader(LOADER.name, LOADER)
probe = importlib.util.module_from_spec(SPEC)
# Register before executing: the module defines dataclasses, and the decorator resolves
# annotations through sys.modules, which a loader-only import leaves without an entry.
sys.modules[LOADER.name] = probe
LOADER.exec_module(probe)

NOW = dt.datetime(2026, 9, 8, 12, 0, tzinfo=dt.timezone.utc)


def response(status, headers=None, body=""):
    return probe.Response(status=status, headers=headers or {}, body_prefix=body)


class ApiVerdict(unittest.TestCase):
    def test_ok_needs_the_body_not_just_the_status(self):
        self.assertEqual(probe.judge_api(response(200, body="ok\n")).state, probe.OK)
        self.assertEqual(probe.judge_api(response(200, body="")).state, probe.DOWN)

    def test_a_readiness_503_is_an_outage_not_a_missing_feature(self):
        # The endpoint returns `unavailable` for exactly one reason: it could not reach the
        # database. Excusing that as "unconfigured" would drop it out of the uptime tally
        # entirely, so the one failure the probe exists to see would be the one never counted.
        outcome = probe.judge_api(response(503, body="unavailable"))
        self.assertEqual(outcome.state, probe.DOWN)
        self.assertIn("database", outcome.detail)

    def test_a_proxy_503_is_not_blamed_on_the_database(self):
        # A reverse proxy with no server behind it answers 503 without the API being involved
        # at all. Same verdict, different machine to go and look at, and a record that named
        # the database would send somebody to the wrong one.
        outcome = probe.judge_api(response(503, {"content-type": "text/html"}))
        self.assertEqual(outcome.state, probe.DOWN)
        self.assertNotIn("database", outcome.detail)
        self.assertIn("did not send", outcome.detail)

    def test_html_names_the_reverse_proxy_rather_than_the_api(self):
        outcome = probe.judge_api(response(200, {"content-type": "text/html; charset=utf-8"}))
        self.assertEqual(outcome.state, probe.DOWN)
        self.assertIn("web app answered", outcome.detail)

    def test_the_diagnostic_survives_an_unusually_cased_media_type(self):
        outcome = probe.judge_api(response(200, {"content-type": "TEXT/HTML"}))
        self.assertIn("web app answered", outcome.detail)


class WebVerdict(unittest.TestCase):
    def test_html_is_the_whole_requirement(self):
        ok = response(200, {"content-type": "text/html; charset=utf-8"})
        self.assertEqual(probe.judge_web(ok).state, probe.OK)

    def test_the_media_type_is_matched_the_way_http_defines_it(self):
        # Case insensitive, and the whole token rather than a prefix. A prefix match would
        # call a legal Text/HTML response an outage and a text/html-extra one healthy, both
        # of which put the wrong cause in a record people are meant to trust.
        upper = response(200, {"content-type": "Text/HTML; charset=UTF-8"})
        self.assertEqual(probe.judge_web(upper).state, probe.OK)
        lookalike = response(200, {"content-type": "text/html-extra"})
        self.assertEqual(probe.judge_web(lookalike).state, probe.DOWN)

    def test_a_200_that_is_not_a_page_is_not_the_app(self):
        plain = response(200, {"content-type": "text/plain"})
        self.assertEqual(probe.judge_web(plain).state, probe.DOWN)
        self.assertEqual(probe.judge_web(response(502)).state, probe.DOWN)


class WrongBaseUrl(unittest.TestCase):
    """A base URL that redirects takes every row down at once while the job stays green and
    the heartbeat keeps pinging, so the external alarm the whole design leans on would be
    confirming health while ninety days of invented downtime accrued. Nothing else in the
    system can tell that apart from a real outage, so the script has to."""

    def run_probe(self, base_url, data_dir, responses=None):
        """Drive `main` end to end. With `responses` the network is stubbed; without it the
        real fetch runs, which is how the outage case below stays a genuine refusal."""
        argv = ["status-probe", "--base-url", base_url, "--data-dir", data_dir]
        with contextlib.ExitStack() as stack:
            stack.enter_context(unittest.mock.patch.object(sys, "argv", argv))
            if responses is not None:
                stack.enter_context(
                    unittest.mock.patch.object(probe, "fetch", lambda _base, p: responses[p.id])
                )
            return probe.main()

    def test_it_refuses_and_writes_nothing_when_every_probe_redirects(self):
        redirect = response(301, {"location": "https://www.example.com/"})
        with tempfile.TemporaryDirectory() as d:
            code = self.run_probe("https://example.com", d, {p.id: redirect for p in probe.PROBES})
            self.assertEqual(code, 1, "a red run is what stops the heartbeat that follows")
            self.assertEqual(list(Path(d).iterdir()), [], "no invented downtime was recorded")

    def test_one_unconfigured_component_cannot_disable_the_refusal(self):
        # Not canary-specific, which is why it is here rather than with the canary tests. Any
        # component that reports itself unconfigured reaches this, and a 503 whose body says
        # "not configured" has been able to since before there was a canary: one such component
        # among misdirected ones made the check unreachable and turned a refusal into ninety days
        # of recorded downtime that never happened.
        redirect = response(301, {"location": "https://www.example.com/"})
        probes = [
            probe.Probe(
                id="astray", name="A", description="", method="GET", path="/a",
                judge=probe.judge_web,
            ),
            probe.Probe(
                id="off", name="B", description="", method="GET", path="/b",
                judge=lambda _r: probe.Outcome(probe.UNCONFIGURED, "switched off"),
            ),
        ]
        with tempfile.TemporaryDirectory() as d:
            with unittest.mock.patch.object(probe, "PROBES", probes):
                with unittest.mock.patch.object(
                    probe, "fetch", lambda _b, _p: redirect
                ):
                    code = self.run_probe("https://example.com", d)
            self.assertEqual(code, 1)
            self.assertEqual(list(Path(d).iterdir()), [])

    def test_a_real_outage_is_still_recorded_and_still_reports_success(self):
        # The distinction that makes the refusal safe: a deployment that is gone refuses
        # connections, it does not politely redirect them. That must keep being written down,
        # and must keep heartbeating, because the collector is working perfectly.
        def refuse(*_args, **_kwargs):
            raise urllib.error.URLError(ConnectionRefusedError(61, "Connection refused"))

        with tempfile.TemporaryDirectory() as d:
            with unittest.mock.patch.object(probe, "fetch", refuse):
                code = self.run_probe("https://example.invalid", d)
            self.assertEqual(code, 0)
            summary = probe.load(d)
            api = next(c for c in summary["components"] if c["id"] == "api")
            self.assertEqual(api["state"], probe.DOWN)
            self.assertEqual(api["days"], [{"date": api["days"][0]["date"], "ok": 0, "total": 1}])

    def test_one_redirect_among_working_probes_is_recorded_not_refused(self):
        # Conservative on purpose. Only every component at once is unambiguous; a single odd
        # row could be a genuinely misrouted path, which is a real problem worth recording.
        responses = {
            "api": response(200, body="ok"),
            "web": response(200, {"content-type": "text/html"}),
            "signin": response(303, {"location": "https://github.com/login/oauth/authorize"}),
            "billing": response(301, {"location": "https://elsewhere.example/"}),
        }
        with tempfile.TemporaryDirectory() as d:
            self.assertEqual(self.run_probe("https://example.com", d, responses), 0)
            billing = next(c for c in probe.load(d)["components"] if c["id"] == "billing")
            self.assertEqual(billing["state"], probe.DOWN)


class Misdirection(unittest.TestCase):
    """A base URL that is not the origin the deployment serves takes every row down at once,
    which is a configuration mistake wearing the costume of a total outage."""

    def redirect(self, to="https://getsotto.co.uk/"):
        return response(301, {"location": to})

    def test_every_probe_that_expects_no_redirect_names_one(self):
        for judge in (probe.judge_api, probe.judge_web, probe.judge_billing):
            with self.subTest(judge=judge.__name__):
                outcome = judge(self.redirect())
                self.assertEqual(outcome.state, probe.DOWN)
                self.assertIn("redirected to https://getsotto.co.uk", outcome.detail)
                self.assertIn("base url", outcome.detail)

    def test_only_the_origin_of_a_redirect_is_recorded(self):
        # This detail is written to a public branch and kept for ninety days. A proxy can put
        # state in a redirect's query, and the origin is the whole of what diagnoses the
        # problem, so nothing after the host is worth the risk of keeping.
        outcome = probe.judge_web(self.redirect("https://example.com/cb?token=sekrit&id=42"))
        self.assertIn("https://example.com", outcome.detail)
        self.assertNotIn("sekrit", outcome.detail)
        self.assertNotIn("?", outcome.detail)

    def test_a_relative_redirect_is_described_rather_than_echoed(self):
        outcome = probe.judge_web(self.redirect("/somewhere?token=sekrit"))
        self.assertNotIn("sekrit", outcome.detail)
        self.assertIn("no origin", outcome.detail)

    def test_sign_in_is_not_caught_by_it(self):
        # The one probe whose healthy answer is a redirect must keep passing.
        r = response(303, {"location": "https://github.com/login/oauth/authorize?client_id=x"})
        self.assertEqual(probe.judge_signin(r).state, probe.OK)

    def test_the_two_checks_agree_on_what_a_redirect_is(self):
        # They read the same list, and they have to. Held apart, a code in one and not the
        # other means a deployment redirecting with it is either wrongly failed or silently
        # excused, depending only on which probe happened to see it.
        github = {"location": "https://github.com/login/oauth/authorize"}
        elsewhere = {"location": "https://www.example.com/"}
        for status in probe.REDIRECTS:
            with self.subTest(status=status):
                self.assertEqual(probe.judge_signin(response(status, github)).state, probe.OK)
                astray = probe.judge_signin(response(status, elsewhere))
                self.assertTrue(astray.fault, "a redirect away from github is a base url fault")
                self.assertTrue(probe.judge_web(response(status, elsewhere)).fault)

    def test_a_readiness_503_still_wins_over_the_redirect_check(self):
        # Ordering matters: a database outage must not be relabelled as a URL problem.
        outcome = probe.judge_api(response(503, body="unavailable"))
        self.assertIn("database", outcome.detail)


class SigninVerdict(unittest.TestCase):
    def test_a_redirect_to_github_is_the_flow_starting(self):
        r = response(303, {"location": "https://github.com/login/oauth/authorize?client_id=x"})
        self.assertEqual(probe.judge_signin(r).state, probe.OK)

    def test_a_redirect_somewhere_else_is_not(self):
        # A deployment sending sign-in traffic anywhere but GitHub is broken at best, and
        # counting it as healthy would hide the more alarming readings of the same symptom.
        r = response(303, {"location": "https://elsewhere.example/login"})
        self.assertEqual(probe.judge_signin(r).state, probe.DOWN)

    def test_no_oauth_credentials_is_a_choice_not_an_outage(self):
        said = response(503, body="oauth is not configured")
        self.assertEqual(probe.judge_signin(said).state, probe.UNCONFIGURED)

    def test_a_proxy_503_is_downtime_rather_than_a_missing_feature(self):
        # The expensive mistake, because unconfigured samples are excluded from the tally
        # rather than counted as bad: a reverse proxy with nothing behind it saying "Service
        # Unavailable" would delete real downtime from a published figure, not just mislabel
        # it. Nothing but the application's own words may excuse a 503.
        proxy = response(503, {"content-type": "text/html"}, "<html>Service Unavailable</html>")
        outcome = probe.judge_signin(proxy)
        self.assertEqual(outcome.state, probe.DOWN)
        self.assertIn("did not send", outcome.detail)

    def test_a_200_means_the_redirect_never_happened(self):
        self.assertEqual(probe.judge_signin(response(200)).state, probe.DOWN)


class BillingVerdict(unittest.TestCase):
    def test_rejecting_an_unsigned_webhook_is_the_healthy_answer(self):
        self.assertEqual(probe.judge_billing(response(401)).state, probe.OK)

    def test_accepting_an_unsigned_webhook_is_never_healthy(self):
        # This probe sends an unsigned payload. A 200 means signature verification is not
        # happening, which is a security failure, and reporting it green on a status page
        # would be the worst possible place to be quiet about it.
        self.assertEqual(probe.judge_billing(response(200)).state, probe.DOWN)

    def test_no_billing_configured_is_a_choice_not_an_outage(self):
        said = response(503, body="billing is not configured")
        self.assertEqual(probe.judge_billing(said).state, probe.UNCONFIGURED)

    def test_a_proxy_503_is_downtime_rather_than_a_missing_feature(self):
        proxy = response(503, {"content-type": "text/html"}, "<html>Service Unavailable</html>")
        outcome = probe.judge_billing(proxy)
        self.assertEqual(outcome.state, probe.DOWN)
        self.assertIn("did not send", outcome.detail)


class Observation(unittest.TestCase):
    def probe_for(self, judge, path="/x"):
        return probe.Probe(id="t", name="T", description="", method="GET", path=path, judge=judge)

    def test_a_transport_failure_is_recorded_as_the_component_being_gone(self):
        # Raised rather than provoked. An earlier version dialled a port it assumed nothing
        # was listening on, which is not true on every machine: a local proxy answered and the
        # test failed on a correct verdict. The exception type is the thing being tested, so
        # it is the thing to supply.
        def refuse(*_args, **_kwargs):
            raise urllib.error.URLError(ConnectionRefusedError(61, "Connection refused"))

        with unittest.mock.patch.object(probe, "fetch", refuse):
            outcomes = probe.observe("https://example.invalid", [self.probe_for(probe.judge_web)])
        self.assertEqual(outcomes["t"].state, probe.DOWN)
        self.assertIn("URLError", outcomes["t"].detail)

    def test_the_caught_types_are_the_ones_a_network_actually_raises(self):
        # URLError is an OSError and a truncated reply is an HTTPException, so both must land
        # in the transport branch rather than escaping as a collector defect.
        self.assertIsInstance(urllib.error.URLError("x"), probe.TRANSPORT_FAILURES)
        self.assertIsInstance(http.client.RemoteDisconnected("x"), probe.TRANSPORT_FAILURES)
        self.assertIsInstance(TimeoutError(), probe.TRANSPORT_FAILURES)

    def test_a_broken_verdict_raises_rather_than_reporting_an_outage(self):
        # The failure this guards against is subtle and bad: a defect in our own code recorded
        # as somebody else's downtime, published on a status page, with the job still green.
        # Nothing would ever have pointed at the collector.
        def broken(_response):
            raise AttributeError("verdict bug")

        with unittest.mock.patch.object(probe, "fetch", return_value=response(200)):
            with self.assertRaises(AttributeError):
                probe.observe("https://example.invalid", [self.probe_for(broken)])


class ExcusedSamples(unittest.TestCase):
    """Every route by which a sample can be dropped from the tally, in one place, because a
    dropped sample is invisible in the published figure in a way a wrong one is not."""

    def test_only_the_application_can_excuse_a_503(self):
        proxy = response(503, {"content-type": "text/html"}, "<html>Service Unavailable</html>")
        for judge in (probe.judge_api, probe.judge_signin, probe.judge_billing):
            with self.subTest(judge=judge.__name__):
                self.assertEqual(judge(proxy).state, probe.DOWN)

    def test_the_application_says_it_in_words_the_probe_reads(self):
        # The server renders NotConfigured as the plain text body of a 503, so these are the
        # exact strings a deployment without oauth or billing returns.
        self.assertEqual(
            probe.judge_signin(response(503, body="oauth is not configured")).state,
            probe.UNCONFIGURED,
        )
        self.assertEqual(
            probe.judge_billing(response(503, body="billing is not configured")).state,
            probe.UNCONFIGURED,
        )


class Summary(unittest.TestCase):
    def test_a_first_round_records_every_component(self):
        summary = probe.merge(probe.empty_summary(), {"api": probe.Outcome(probe.OK)}, NOW)
        ids = [c["id"] for c in summary["components"]]
        self.assertEqual(ids, ["api", "web", "signin", "billing", "sync"])
        self.assertEqual(summary["generated_at"], "2026-09-08T12:00:00Z")

    def test_tallies_accumulate_across_rounds(self):
        summary = probe.empty_summary()
        summary = probe.merge(summary, {"api": probe.Outcome(probe.OK)}, NOW)
        summary = probe.merge(summary, {"api": probe.Outcome(probe.DOWN, "boom")}, NOW)
        api = next(c for c in summary["components"] if c["id"] == "api")
        self.assertEqual(api["days"], [{"date": "2026-09-08", "ok": 1, "total": 2}])
        self.assertEqual(api["state"], probe.DOWN)
        self.assertEqual(api["detail"], "boom")

    def test_an_unconfigured_component_never_enters_the_tally(self):
        # Otherwise every self-hoster running without billing would watch their published
        # uptime fall for a feature they deliberately do not run.
        unconfigured = {"billing": probe.Outcome(probe.UNCONFIGURED)}
        summary = probe.merge(probe.empty_summary(), unconfigured, NOW)
        billing = next(c for c in summary["components"] if c["id"] == "billing")
        self.assertEqual(billing["days"], [])
        self.assertEqual(billing["state"], probe.UNCONFIGURED)

    def test_a_component_with_no_probe_is_declared_not_omitted(self):
        summary = probe.merge(probe.empty_summary(), {}, NOW)
        sync = next(c for c in summary["components"] if c["id"] == "sync")
        self.assertEqual(sync["state"], probe.UNCONFIGURED)
        self.assertEqual(sync["days"], [])

    def test_history_keeps_ageing_after_a_component_stops_being_probed(self):
        # A deployment that drops Stripe stops producing conclusive billing samples but keeps
        # the days it already has. If those only aged while samples arrived, the row would
        # freeze at the moment it went quiet and the summary would keep publishing days from
        # outside the window it claims to hold.
        summary = probe.empty_summary()
        for i in range(probe.RETAINED_DAYS + 5):
            when = NOW - dt.timedelta(days=probe.RETAINED_DAYS + 4 - i)
            summary = probe.merge(summary, {"billing": probe.Outcome(probe.OK)}, when)
        for i in range(30):
            unconfigured = {"billing": probe.Outcome(probe.UNCONFIGURED)}
            summary = probe.merge(summary, unconfigured, NOW + dt.timedelta(days=i))

        billing = next(c for c in summary["components"] if c["id"] == "billing")
        last = (NOW + dt.timedelta(days=29)).date()
        horizon = (last - dt.timedelta(days=probe.RETAINED_DAYS - 1)).isoformat()
        self.assertEqual(billing["days"][0]["date"], horizon)

    def test_days_older_than_the_horizon_fall_off(self):
        days = {
            "2026-01-01": {"date": "2026-01-01", "ok": 1, "total": 1},
            "2026-09-08": {"date": "2026-09-08", "ok": 1, "total": 1},
        }
        kept = probe.trim(days, NOW.date())
        self.assertEqual([d["date"] for d in kept], ["2026-09-08"])

    def test_the_horizon_keeps_exactly_the_retained_window(self):
        oldest = NOW.date() - dt.timedelta(days=probe.RETAINED_DAYS - 1)
        days = {d.isoformat(): {"date": d.isoformat(), "ok": 1, "total": 1}
                for d in (oldest - dt.timedelta(days=1), oldest, NOW.date())}
        kept = [d["date"] for d in probe.trim(days, NOW.date())]
        self.assertIn(oldest.isoformat(), kept)
        self.assertNotIn((oldest - dt.timedelta(days=1)).isoformat(), kept)


class Samples(unittest.TestCase):
    def test_one_line_per_observation_carrying_its_own_timestamp(self):
        # The cadence of this job is whatever GitHub's scheduler decides on the day, so a
        # sample that did not carry its own time could only be placed by assuming one.
        outcomes = {"api": probe.Outcome(probe.OK), "web": probe.Outcome(probe.DOWN, "x")}
        lines = probe.sample_lines(outcomes, NOW)
        parsed = [json.loads(line) for line in lines]
        self.assertEqual([p["component"] for p in parsed], ["api", "web"])
        self.assertEqual(parsed[0]["at"], "2026-09-08T12:00:00Z")
        self.assertEqual(parsed[1]["detail"], "x")


class Retention(unittest.TestCase):
    def test_sample_files_age_out_with_the_summary(self):
        old = (NOW.date() - dt.timedelta(days=probe.RETAINED_DAYS)).isoformat()
        edge = (NOW.date() - dt.timedelta(days=probe.RETAINED_DAYS - 1)).isoformat()
        names = [f"{old}.jsonl", f"{edge}.jsonl", f"{NOW.date().isoformat()}.jsonl"]
        self.assertEqual(probe.expired_samples(names, NOW.date()), [f"{old}.jsonl"])

    def test_nothing_it_did_not_write_is_deleted(self):
        # This runs `rm` inside a directory on a branch it pushes. A file it does not
        # recognise is somebody else's, and guessing wrong here destroys data.
        names = ["README.md", "notes.txt", "2026-13-45.jsonl", "backup.jsonl.gz"]
        self.assertEqual(probe.expired_samples(names, NOW.date()), [])

    def test_pruning_leaves_the_current_day_alone(self):
        with tempfile.TemporaryDirectory() as d:
            outcomes = {"api": probe.Outcome(probe.OK)}
            summary = probe.merge(probe.load(d), outcomes, NOW)
            probe.write(d, summary, probe.sample_lines(outcomes, NOW), NOW)
            stale = Path(d) / "samples" / "2020-01-01.jsonl"
            stale.write_text("{}\n")
            probe.prune(d, NOW.date())
            self.assertFalse(stale.exists())
            self.assertTrue((Path(d) / "samples" / "2026-09-08.jsonl").exists())


class Persistence(unittest.TestCase):
    def test_a_missing_summary_starts_empty_rather_than_failing(self):
        with tempfile.TemporaryDirectory() as d:
            self.assertEqual(probe.load(d)["components"], [])

    def test_a_second_run_appends_rather_than_replacing(self):
        with tempfile.TemporaryDirectory() as d:
            for _ in range(2):
                outcomes = {"api": probe.Outcome(probe.OK)}
                summary = probe.merge(probe.load(d), outcomes, NOW)
                probe.write(d, summary, probe.sample_lines(outcomes, NOW), NOW)
            api = next(c for c in probe.load(d)["components"] if c["id"] == "api")
            self.assertEqual(api["days"], [{"date": "2026-09-08", "ok": 2, "total": 2}])
            lines = (Path(d) / "samples" / "2026-09-08.jsonl").read_text().strip().split("\n")
            self.assertEqual(len(lines), 2)


if __name__ == "__main__":
    unittest.main()
