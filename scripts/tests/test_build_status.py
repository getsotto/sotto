"""Status page shaping: what the page is allowed to claim about what was observed.

Most of these guard one property. A status page is believed or it is useless, so the failures
worth testing are the ones where it would say something confident that the data does not
support: a day nobody checked drawn as a good day, a percentage with no denominator, an
unconfigured component voting on whether the service is up.
"""

import base64
import contextlib
import datetime as dt
import importlib.machinery
import importlib.util
import io
import re
import sys
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
LOADER = importlib.machinery.SourceFileLoader("build_status", str(ROOT / "scripts/build-status"))
SPEC = importlib.util.spec_from_loader(LOADER.name, LOADER)
page = importlib.util.module_from_spec(SPEC)
sys.modules[LOADER.name] = page
LOADER.exec_module(page)

TODAY = dt.date(2026, 9, 9)


def component(state, days=None, **kw):
    return {"id": "x", "name": "X", "state": state, "days": days or [], **kw}


class Banner(unittest.TestCase):
    def test_everything_measured_and_up(self):
        self.assertEqual(page.overall([component("ok"), component("ok")]), page.OPERATIONAL)

    def test_one_down_is_partial_and_all_down_is_major(self):
        self.assertEqual(page.overall([component("ok"), component("down")]), page.PARTIAL)
        self.assertEqual(page.overall([component("down"), component("down")]), page.MAJOR)

    def test_an_unconfigured_component_does_not_vote(self):
        # A deployment that has chosen not to run billing is not having a billing outage, and
        # must not be shown one.
        self.assertEqual(page.overall([component("ok"), component("unconfigured")]),
                         page.OPERATIONAL)

    def test_nothing_measured_is_unknown_rather_than_fine(self):
        # The dangerous default. Before the first check lands, or if every probe is
        # unconfigured, there is no evidence of health, and "All systems operational" would be
        # an assertion nobody made.
        self.assertEqual(page.overall([component("unconfigured")]), page.UNKNOWN)
        self.assertEqual(page.overall([]), page.UNKNOWN)


class Bars(unittest.TestCase):
    def test_a_day_nobody_checked_is_blank_not_green(self):
        # The property the whole page rests on. The collector runs when GitHub gets round to
        # it, so gaps are normal; drawing them as good days would invent uptime, and drawing
        # them as bad ones would invent outages.
        slots = page.day_slots([{"date": "2026-09-09", "ok": 2, "total": 2}], TODAY)
        self.assertEqual(len(slots), page.SPAN_DAYS)
        self.assertIsNone(slots[0]["tally"], "the oldest day has no samples")
        self.assertEqual(page.bar_class(slots[0]["tally"]), "none")
        self.assertEqual(slots[-1]["date"], "2026-09-09")
        self.assertEqual(page.bar_class(slots[-1]["tally"]), "")

    def test_a_partly_failing_day_is_neither_colour(self):
        self.assertEqual(page.bar_class({"ok": 1, "total": 2}), "partial")
        self.assertEqual(page.bar_class({"ok": 0, "total": 2}), "down")

    def test_a_partly_failing_day_is_drawn_in_proportion(self):
        # One failure in ten and nine in ten are different days; a fixed half tells the reader
        # they were the same.
        self.assertIn("10%", page.bar_style({"ok": 9, "total": 10}))
        self.assertIn("90%", page.bar_style({"ok": 1, "total": 10}))
        self.assertEqual(page.bar_style({"ok": 4, "total": 4}), "", "a clean day needs no shading")
        self.assertEqual(page.bar_style(None), "")

    def test_the_hover_text_carries_the_counts(self):
        slot = {"date": "2026-09-09", "tally": {"ok": 1, "total": 4}}
        self.assertEqual(page.bar_title(slot), "2026-09-09: 1 of 4 checks passed")
        self.assertIn("not checked", page.bar_title({"date": "2026-01-01", "tally": None}))


class Uptime(unittest.TestCase):
    def test_the_denominator_survives(self):
        up = page.uptime([{"date": "d", "ok": 3, "total": 4}])
        self.assertEqual((up["ok"], up["total"]), (3, 4))
        self.assertAlmostEqual(up["percent"], 75.0)

    def test_the_percentage_counts_only_what_the_bars_can_show(self):
        # Ten failures from outside the window and one success inside it. The bars can draw
        # exactly one day, so a percentage built from eleven checks describes a chart nobody
        # is looking at, and describes it as an outage.
        days = [{"date": "2026-01-01", "ok": 0, "total": 10},
                {"date": "2026-09-09", "ok": 1, "total": 1}]
        model = page.build({"generated_at": "x",
                            "components": [{"id": "a", "name": "A", "state": "ok",
                                            "days": days}]}, [], TODAY)
        row = model["rows"][0]
        self.assertEqual(row["uptime"], {"ok": 1, "total": 1, "percent": 100.0})
        self.assertEqual(len([s for s in row["slots"] if s["tally"]]), 1)

    def test_no_checks_is_not_zero_percent(self):
        # Zero would read as a total outage. There is simply nothing to report.
        self.assertIsNone(page.uptime([]))

    def test_cadence_is_measured_not_assumed(self):
        # The collector asks for one check every ten minutes and does not get it. Whatever the
        # page says about how often it looks has to come from the data.
        days = [{"date": "2026-09-08", "ok": 2, "total": 2},
                {"date": "2026-09-09", "ok": 4, "total": 4}]
        self.assertEqual(page.observed_cadence(days, TODAY), 3.0)
        self.assertIsNone(page.observed_cadence([], TODAY))

    def test_the_days_it_did_not_run_are_in_the_divisor(self):
        # The flattering error. Counting only days that have a sample lets two bursts a month
        # apart report the same density as two consecutive days, because the empty month
        # between them never enters the sum.
        bursts = [{"date": "2026-08-10", "ok": 2, "total": 2},
                  {"date": "2026-09-09", "ok": 2, "total": 2}]
        self.assertAlmostEqual(page.observed_cadence(bursts, TODAY), 4 / 31)

    def test_a_young_collector_is_not_charged_for_days_before_it_existed(self):
        # The other direction, and why this is not simply divided by the ninety day window: a
        # collector two days old has not missed eighty-eight days, it has not lived through
        # them, and reporting 0.04 checks a day would be its own kind of lie.
        days = [{"date": "2026-09-08", "ok": 2, "total": 2},
                {"date": "2026-09-09", "ok": 2, "total": 2}]
        self.assertEqual(page.observed_cadence(days, TODAY), 2.0)


class Incidents(unittest.TestCase):
    def test_the_stage_comes_from_the_labels(self):
        issue = {"title": "Sync is slow", "state": "OPEN", "createdAt": "2026-09-01T10:00:00Z",
                 "labels": [{"name": "incident"}, {"name": "identified"}], "comments": []}
        self.assertEqual(page.parse_incidents([issue], TODAY)[0]["stage"], "identified")

    def test_the_state_is_read_however_github_spells_it(self):
        # `gh --json state` emits CLOSED; the REST API emits closed. Matching one of them
        # exactly is a silent failure in the worst direction, publishing a resolved incident as
        # still happening and keeping it on the page for ever.
        for spelling in ("CLOSED", "closed"):
            with self.subTest(spelling=spelling):
                issue = {"title": "Done", "state": spelling, "labels": [], "comments": [],
                         "createdAt": "2026-09-08T10:00:00Z"}
                self.assertEqual(page.parse_incidents([issue], TODAY)[0]["stage"], "resolved")

    def test_closing_the_issue_resolves_it_whatever_the_labels_say(self):
        # Otherwise an incident closed without tidying its labels would sit on the page
        # claiming to be under investigation for ever.
        issue = {"title": "Outage", "state": "CLOSED", "createdAt": "2026-09-01T10:00:00Z",
                 "labels": [{"name": "investigating"}], "comments": []}
        self.assertEqual(page.parse_incidents([issue], TODAY)[0]["stage"], "resolved")

    def test_comments_become_the_updates_in_order(self):
        issue = {"title": "Outage", "state": "OPEN", "createdAt": "2026-09-01T10:00:00Z",
                 "labels": [], "comments": [
                     {"createdAt": "2026-09-01T10:30:00Z", "body": "Looking into it"},
                     {"createdAt": "2026-09-01T11:00:00Z", "body": "Fixed"}]}
        updates = page.parse_incidents([issue], TODAY)[0]["updates"]
        self.assertEqual([u["body"] for u in updates], ["Looking into it", "Fixed"])
        self.assertEqual(updates[0]["at"], "2026-09-01 10:30")

    def test_newest_first_even_on_the_same_day(self):
        # Two incidents on one bad day is exactly when the order matters, and exactly when
        # sorting on the date alone stops distinguishing them.
        def issue(title, at):
            return {"title": title, "createdAt": at, "labels": [], "comments": []}

        issues = [issue("Morning", "2026-09-08T09:00:00Z"),
                  issue("Evening", "2026-09-08T21:00:00Z"),
                  issue("Yesterday", "2026-09-07T12:00:00Z")]
        self.assertEqual([i["title"] for i in page.parse_incidents(issues, TODAY)],
                         ["Evening", "Morning", "Yesterday"])

    def test_updates_are_ordered_however_the_api_returned_them(self):
        issue = {"title": "Outage", "state": "OPEN", "createdAt": "2026-09-01T10:00:00Z",
                 "labels": [], "comments": [
                     {"createdAt": "2026-09-01T12:00:00Z", "body": "Resolved"},
                     {"createdAt": "2026-09-01T10:30:00Z", "body": "Looking into it"}]}
        updates = page.parse_incidents([issue], TODAY)[0]["updates"]
        self.assertEqual([u["body"] for u in updates], ["Looking into it", "Resolved"])

    def test_a_resolved_label_does_not_resolve_an_open_incident(self):
        # Closing the issue is what resolves an incident. Reading it from a label would let a
        # stale one publish an outage as over while it was still happening.
        issue = {"title": "Ongoing", "state": "OPEN", "createdAt": "2026-09-08T10:00:00Z",
                 "labels": [{"name": "resolved"}], "comments": []}
        self.assertEqual(page.parse_incidents([issue], TODAY)[0]["stage"], "investigating")

    def test_an_incident_resolved_inside_the_window_is_kept(self):
        # It opened before the window and ended inside it, which is precisely the shape of a
        # long outage. The bars will be showing those days; the log has to explain them.
        long_one = {"title": "Long outage", "state": "CLOSED", "labels": [], "comments": [],
                    "createdAt": "2026-05-01T00:00:00Z", "closedAt": "2026-09-05T00:00:00Z"}
        titles = [i["title"] for i in page.parse_incidents([long_one], TODAY)]
        self.assertEqual(titles, ["Long outage"])

    def test_old_closed_incidents_age_out_but_open_ones_never_do(self):
        # The bars cover ninety days, so the log does too. The exception is the one that
        # matters: ageing out an incident that is still happening would be the worst thing
        # this page could do.
        stale = {"title": "Long resolved", "state": "CLOSED", "labels": [], "comments": [],
                 "createdAt": "2025-01-01T00:00:00Z", "closedAt": "2025-01-02T00:00:00Z"}
        ancient_open = {"title": "Still open", "state": "OPEN", "labels": [], "comments": [],
                        "createdAt": "2025-01-01T00:00:00Z"}
        titles = [i["title"] for i in page.parse_incidents([stale, ancient_open], TODAY)]
        self.assertEqual(titles, ["Still open"])


class Robustness(unittest.TestCase):
    def test_a_component_with_no_name_does_not_take_the_build_down(self):
        # The page is built from a file another job writes. A missing field should cost a
        # label, not the whole status page at the moment somebody needs it.
        model = page.build({"generated_at": "x", "components": [{"state": "ok"}]}, [], TODAY)
        self.assertEqual(model["rows"][0]["name"], "Unnamed")


class Rendering(unittest.TestCase):
    def summary(self, **kw):
        return {"generated_at": "2026-09-09T05:05:01Z",
                "components": [component("ok", [{"date": "2026-09-09", "ok": 4, "total": 4}],
                                         description="D", **kw)]}

    def test_the_page_states_the_denominator_and_not_only_the_percentage(self):
        out = page.render(page.build(self.summary(), [], TODAY),
                          dt.datetime(2026, 9, 9, tzinfo=dt.timezone.utc))
        self.assertIn("100.00% of 4 checks", out)
        self.assertIn("sampled, not measured continuously", out)
        self.assertIn("shown blank rather than green", out)

    def test_the_page_carries_its_icon_rather_than_linking_to_one(self):
        out = page.render(page.build(self.summary(), [], TODAY),
                          dt.datetime(2026, 9, 9, tzinfo=dt.timezone.utc))
        self.assertIn('rel="icon"', out)
        self.assertIn("data:image/svg+xml;base64,", out)

    def test_the_icon_is_the_web_app_s_own_and_not_a_second_copy(self):
        # The anti-drift property. Pasting the bytes in would work today and be wrong the first
        # time somebody redesigns the app's icon, leaving the status page showing the old one
        # with nothing to notice.
        inline = page.favicon_link().split("base64,")[1].rstrip('">')
        self.assertEqual(base64.b64decode(inline), page.FAVICON_SOURCE.read_bytes())

    def test_nothing_on_the_page_is_fetched_from_anywhere_else(self):
        # The property the workflow's own comment claims: "no scripts and no external assets, so
        # it has no failure domain of its own". A status page that fetches its icon from the
        # service it reports on loses the icon on the one morning anyone looks.
        out = page.render(page.build(self.summary(), [], TODAY),
                          dt.datetime(2026, 9, 9, tzinfo=dt.timezone.utc))
        for subresource in ("<script", 'rel="stylesheet"', "<img", "@import", "url(http"):
            self.assertNotIn(subresource, out, subresource)
        for href in re.findall(r'<link[^>]*href="([^"]*)"', out):
            self.assertTrue(href.startswith("data:"), f"a link fetches {href}")

    def test_a_missing_icon_does_not_take_the_page_down_with_it(self):
        # Decoration must not be able to stop a status page publishing. It degrades, loudly
        # enough to see in a build log and quietly enough to still serve the page.
        missing = page.FAVICON_SOURCE.parent / "does-not-exist.svg"
        stderr = io.StringIO()
        with contextlib.redirect_stderr(stderr):
            self.assertEqual(page.favicon_link(missing), "")
        self.assertIn("the page will have none", stderr.getvalue())

    def test_content_from_the_collector_is_escaped(self):
        # Details are written by the probe and can quote a redirect target or an error, so they
        # are not this page's to trust; a status page that could be scripted by a misbehaving
        # deployment would be a poor thing to publish.
        model = page.build(self.summary(detail='<img src=x onerror="alert(1)">'), [], TODAY)
        out = page.render(model, dt.datetime(2026, 9, 9, tzinfo=dt.timezone.utc))
        self.assertNotIn("<img src=x", out)
        self.assertIn("&lt;img src=x", out)

    def test_a_list_within_budget_never_warns(self):
        self.assertFalse(page.possibly_truncated([{}] * 3, 200))
        self.assertFalse(page.possibly_truncated([{}] * 200, 200), "exactly full is not over")

    def test_one_more_than_the_budget_warns(self):
        # The fetch asks for budget + 1 precisely so this is a fact rather than a guess.
        self.assertTrue(page.possibly_truncated([{}] * 201, 200))

    def test_an_old_incident_resolved_recently_cannot_be_reasoned_away(self):
        # The trap in the cleverer version this replaced. These all opened long before the
        # window and are all displayed anyway, because they were resolved inside it, so a
        # judgement based on opening dates would have called a truncated list complete.
        issues = [{"createdAt": "2025-01-01T00:00:00Z", "closedAt": "2026-09-05T00:00:00Z",
                   "state": "CLOSED", "labels": [], "comments": [], "title": "x"}] * 6
        self.assertEqual(len(page.parse_incidents(issues, TODAY)), 6, "all of them display")
        self.assertTrue(page.possibly_truncated(issues, 5))

    def test_a_full_fetch_is_reported_as_possibly_incomplete(self):
        # A short list that does not say it is short is the one failure a page about honesty
        # cannot afford.
        model = page.build(self.summary(), [], TODAY, truncated=True)
        out = page.render(model, dt.datetime(2026, 9, 9, tzinfo=dt.timezone.utc))
        self.assertIn("the list above is incomplete", out)
        self.assertNotIn("incomplete", page.render(page.build(self.summary(), [], TODAY),
                                                   dt.datetime(2026, 9, 9, tzinfo=dt.timezone.utc)))

    def test_a_zero_is_rendered_rather_than_blanked(self):
        # `text or ""` would turn a legitimate 0 into nothing at all.
        self.assertEqual(page.e(0), "0")
        self.assertEqual(page.e(False), "False")
        self.assertEqual(page.e(None), "")

    def test_an_unreadable_incident_log_is_not_an_empty_one(self):
        # The one sentence on this page nobody should read without it being true. Swallowing a
        # failed fetch into an empty list publishes "no incidents" during exactly the outage
        # that caused the fetch to fail.
        model = page.build(self.summary(), [], TODAY)
        model["incidents_unavailable"] = True
        out = page.render(model, dt.datetime(2026, 9, 9, tzinfo=dt.timezone.utc))
        self.assertIn("could not be read", out)
        self.assertNotIn("No incidents in the last 90 days", out)

    def test_it_says_so_when_nothing_has_been_observed(self):
        model = page.build({"generated_at": "x", "components": [component("unconfigured")]},
                           [], TODAY)
        out = page.render(model, dt.datetime(2026, 9, 9, tzinfo=dt.timezone.utc))
        self.assertIn("Status unknown", out)
        self.assertNotIn("All systems operational", out)


if __name__ == "__main__":
    unittest.main()
