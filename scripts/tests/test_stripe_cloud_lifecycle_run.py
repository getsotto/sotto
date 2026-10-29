from __future__ import annotations

import importlib.util
from importlib.machinery import SourceFileLoader
import io
import json
import os
from pathlib import Path
import tempfile
import time
import unittest
from unittest import mock

ROOT = Path(__file__).resolve().parents[2]
LOADER = SourceFileLoader("stripe_cloud_lifecycle_run", str(ROOT / "scripts/stripe-cloud-lifecycle-run"))
SPEC = importlib.util.spec_from_loader(LOADER.name, LOADER)
MODULE = importlib.util.module_from_spec(SPEC)
LOADER.exec_module(MODULE)


class FakeStripeClient:
    def __init__(self):
        self._api_key = "rk_test_not-a-real-key"
        self.calls = []
        self.fail_customer_once = False
        self.customer_create_error = None
        self.has_subscription = False
        self.customer_owner = True
        self.listed_clock = False
        self.listed_customer = False
        self.duplicate_clock = False
        self.duplicate_customer = False
        self.customer_deleted = False
        self.clock_deleted = False
        self.fail_customer_delete_after_apply = False
        self.fail_clock_delete_after_apply = False
        self.clock_frozen_time_offset = 0
        self.read_clock_frozen_time_offset = 0
        self.created_clock_frozen_time = None
        self.created_clock_name = None
        self.created_customer_run_id = None

    def request(self, method, path, fields=None, *, idempotency_key=None):
        self.calls.append((method, path, fields, idempotency_key))
        if method == "GET" and path == "/v1/account":
            return {"id": "acct_123"}
        if method == "POST" and path == "/v1/test_helpers/test_clocks":
            self.created_clock_frozen_time = fields["frozen_time"]
            self.created_clock_name = fields["name"]
            return {
                "id": "clock_test123", "object": "test_helpers.test_clock", "livemode": False,
                "status": "ready", "name": fields["name"],
                "frozen_time": fields["frozen_time"] + self.clock_frozen_time_offset,
            }
        if method == "POST" and path == "/v1/customers":
            self.created_customer_run_id = fields["metadata[sotto_lifecycle_run_id]"]
            if self.customer_create_error is not None:
                error = self.customer_create_error
                self.customer_create_error = None
                raise MODULE.RunError(error)
            if self.fail_customer_once:
                self.fail_customer_once = False
                raise MODULE.RunError("stripe_request_unavailable")
            return {
                "id": "cus_test123", "object": "customer", "livemode": False,
                "test_clock": fields["test_clock"],
                "metadata": {"sotto_lifecycle_run_id": fields["metadata[sotto_lifecycle_run_id]"]},
            }
        if method == "GET" and path == "/v1/customers/cus_test123":
            if self.customer_deleted:
                return {"id": "cus_test123", "object": "customer", "deleted": True}
            return {
                "id": "cus_test123", "object": "customer", "livemode": False,
                "test_clock": "clock_test123",
                "metadata": {"sotto_lifecycle_run_id": self.created_customer_run_id or "11111111-1111-4111-8111-111111111111"}
                if self.customer_owner else {},
            }
        if method == "GET" and path == "/v1/test_helpers/test_clocks":
            rows = []
            if self.listed_clock and not self.clock_deleted:
                rows.append({
                    "id": "clock_test123", "object": "test_helpers.test_clock",
                    "livemode": False,
                    "name": "sotto-lifecycle-11111111-1111-4111-8111-111111111111",
                })
            if self.duplicate_clock and not self.clock_deleted:
                rows.append({
                    "id": "clock_test456", "object": "test_helpers.test_clock",
                    "livemode": False,
                    "name": "sotto-lifecycle-11111111-1111-4111-8111-111111111111",
                })
            return {"object": "list", "data": rows, "has_more": False}
        if method == "GET" and path == "/v1/customers":
            rows = []
            if self.listed_customer:
                rows.append({
                    "id": "cus_test123", "object": "customer", "livemode": False,
                    "test_clock": "clock_test123",
                    "metadata": {"sotto_lifecycle_run_id": "11111111-1111-4111-8111-111111111111"},
                })
            if self.duplicate_customer:
                rows.append({
                    "id": "cus_test456", "object": "customer", "livemode": False,
                    "test_clock": "clock_test123",
                    "metadata": {"sotto_lifecycle_run_id": "11111111-1111-4111-8111-111111111111"},
                })
            return {"object": "list", "data": rows, "has_more": False}
        if method == "GET" and path in {
            "/v1/customers/cus_test123/payment_methods",
            "/v1/customers/cus_test123/sources",
        }:
            return {"object": "list", "data": [], "has_more": False}
        if method == "GET" and path == "/v1/subscriptions":
            return {"object": "list", "data": ([{"id": "sub_existing"}] if self.has_subscription else []), "has_more": False}
        if method == "GET" and path == "/v1/test_helpers/test_clocks/clock_test123":
            if self.clock_deleted:
                raise MODULE.RunError("stripe_resource_not_found")
            return {
                "id": "clock_test123", "object": "test_helpers.test_clock", "livemode": False,
                "status": "ready", "name": self.created_clock_name or "sotto-lifecycle-11111111-1111-4111-8111-111111111111",
                "frozen_time": (self.created_clock_frozen_time or int(time.time()) - 300)
                + self.read_clock_frozen_time_offset,
            }
        if method == "DELETE" and path == "/v1/customers/cus_test123":
            self.customer_deleted = True
            if self.fail_customer_delete_after_apply:
                self.fail_customer_delete_after_apply = False
                raise MODULE.RunError("stripe_request_unavailable")
            return {"id": "cus_test123", "object": "customer", "deleted": True}
        if method == "DELETE" and path == "/v1/test_helpers/test_clocks/clock_test123":
            self.clock_deleted = True
            if self.fail_clock_delete_after_apply:
                self.fail_clock_delete_after_apply = False
                raise MODULE.RunError("stripe_request_unavailable")
            return {"id": "clock_test123", "object": "test_helpers.test_clock", "deleted": True}
        raise AssertionError((method, path, fields))


def manifest():
    started_at_epoch = int(time.time())
    return {
        "schema": MODULE.MANIFEST_SCHEMA,
        "run_id": "11111111-1111-4111-8111-111111111111",
        "started_at_epoch": started_at_epoch,
        "started_at": MODULE.datetime.fromtimestamp(
            started_at_epoch, MODULE.timezone.utc,
        ).isoformat().replace("+00:00", "Z"),
        "app_sha": "a" * 40,
        "api_version": MODULE.PREFLIGHT.server_api_version(),
        "account_fingerprint": MODULE.fingerprint("acct_123"),
        "frozen_time": int(time.time()) - 300,
        "status": "preparing",
        "clock": {
            "id": None, "created": False, "deleted": False, "create_attempted": False,
        },
        "customer": {
            "id": None, "created": False, "deleted": False, "create_attempted": False,
        },
    }


class StripeLifecycleRun(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.run_dir = Path(self.temp.name) / "run"
        self.run_dir.mkdir(mode=0o700)
        self.client = FakeStripeClient()

    def tearDown(self):
        self.temp.cleanup()

    def test_prepare_records_scoped_clock_and_empty_customer_privately(self):
        current = int(time.time()) - 300
        with mock.patch.object(MODULE.PREFLIGHT, "run_preflight") as preflight:
            prepared = MODULE.make_run(self.run_dir, current, self.client, {}, "acct_123", "a" * 40)

        self.assertEqual(prepared["status"], "prepared")
        self.assertEqual(prepared["clock"]["id"], "clock_test123")
        self.assertEqual(prepared["customer"]["id"], "cus_test123")
        self.assertEqual(preflight.call_count, 1)
        self.assertEqual([call[1] for call in self.client.calls], [
            "/v1/test_helpers/test_clocks", "/v1/test_helpers/test_clocks/clock_test123",
            "/v1/customers", "/v1/customers/cus_test123",
        ])
        self.assertTrue(all(
            call[3].startswith("sotto-lifecycle:")
            for call in self.client.calls if call[0] == "POST"
        ))
        file = self.run_dir / "run.json"
        self.assertEqual(file.stat().st_mode & 0o777, 0o600)
        self.assertIn("cus_test123", file.read_text())

    def test_resume_reuses_stable_keys_after_customer_create_timeout(self):
        value = manifest()
        MODULE.private_atomic_json(self.run_dir / "run.json", value)
        self.client.fail_customer_once = True
        with self.assertRaisesRegex(MODULE.RunError, "stripe_request_unavailable"):
            MODULE.resume_run(self.run_dir, value, self.client)
        customer_key = next(call[3] for call in self.client.calls if call[1] == "/v1/customers")

        resumed = MODULE.resume_run(self.run_dir, MODULE.read_manifest(self.run_dir), self.client)
        self.assertEqual(resumed["status"], "prepared")
        customer_posts = [call for call in self.client.calls if call[1] == "/v1/customers"]
        self.assertEqual(customer_posts[1][3], customer_key)

    def test_definite_customer_create_rejection_clears_uncertain_create_marker(self):
        value = manifest()
        MODULE.private_atomic_json(self.run_dir / "run.json", value)
        self.client.customer_create_error = "stripe_resource_not_found"

        with self.assertRaisesRegex(MODULE.RunError, "stripe_resource_not_found"):
            MODULE.resume_run(self.run_dir, value, self.client)

        saved = MODULE.read_manifest(self.run_dir)
        self.assertTrue(saved["clock"]["created"])
        self.assertFalse(saved["customer"]["create_attempted"])

    def test_cleanup_deletes_only_empty_verified_run_resources(self):
        value = manifest()
        value["clock"] = {
            "id": "clock_test123", "created": True, "deleted": False,
            "create_attempted": False,
        }
        value["customer"] = {
            "id": "cus_test123", "created": True, "deleted": False,
            "create_attempted": False,
        }
        MODULE.private_atomic_json(self.run_dir / "run.json", value)

        cleaned = MODULE.cleanup_run(self.run_dir, value, self.client)

        self.assertEqual(cleaned["status"], "cleaned")
        self.assertTrue(cleaned["customer"]["deleted"])
        self.assertTrue(cleaned["clock"]["deleted"])
        self.assertEqual([call[0:2] for call in self.client.calls], [
            ("GET", "/v1/customers/cus_test123"),
            ("GET", "/v1/customers/cus_test123/payment_methods"),
            ("GET", "/v1/customers/cus_test123/sources"),
            ("GET", "/v1/subscriptions"),
            ("DELETE", "/v1/customers/cus_test123"),
            ("GET", "/v1/test_helpers/test_clocks/clock_test123"),
            ("DELETE", "/v1/test_helpers/test_clocks/clock_test123"),
        ])

    def test_cleanup_refuses_customer_with_any_subscription_without_deleting(self):
        value = manifest()
        value["clock"] = {
            "id": "clock_test123", "created": True, "deleted": False,
            "create_attempted": False,
        }
        value["customer"] = {
            "id": "cus_test123", "created": True, "deleted": False,
            "create_attempted": False,
        }
        self.client.has_subscription = True

        with self.assertRaisesRegex(MODULE.RunError, "customer_has_unmanaged_subscriptions"):
            MODULE.cleanup_run(self.run_dir, value, self.client)
        self.assertFalse(any(call[0] == "DELETE" for call in self.client.calls))

    def test_cleanup_refuses_customer_with_saved_payment_method(self):
        value = manifest()
        value["clock"] = {
            "id": "clock_test123", "created": True, "deleted": False,
            "create_attempted": False,
        }
        value["customer"] = {
            "id": "cus_test123", "created": True, "deleted": False,
            "create_attempted": False,
        }
        self.client.request = mock.Mock(side_effect=[
            {
                "id": "cus_test123", "livemode": False, "object": "customer",
                "test_clock": "clock_test123",
                "metadata": {"sotto_lifecycle_run_id": value["run_id"]},
            },
            {"object": "list", "data": [{"id": "pm_saved"}], "has_more": False},
        ])

        with self.assertRaisesRegex(MODULE.RunError, "customer_has_saved_payment_instruments"):
            MODULE.cleanup_run(self.run_dir, value, self.client)
        self.assertFalse(any(call.args[0] == "DELETE" for call in self.client.request.call_args_list))

    def test_cleanup_refuses_customer_whose_run_metadata_does_not_match(self):
        value = manifest()
        value["clock"] = {
            "id": "clock_test123", "created": True, "deleted": False,
            "create_attempted": False,
        }
        value["customer"] = {
            "id": "cus_test123", "created": True, "deleted": False,
            "create_attempted": False,
        }
        self.client.customer_owner = False

        with self.assertRaisesRegex(MODULE.RunError, "customer_cleanup_ownership_mismatch"):
            MODULE.cleanup_run(self.run_dir, value, self.client)
        self.assertFalse(any(call[0] == "DELETE" for call in self.client.calls))

    def test_cleanup_recovers_ids_when_create_succeeded_before_manifest_write(self):
        value = manifest()
        value["clock"]["create_attempted"] = True
        value["customer"]["create_attempted"] = True
        self.client.listed_clock = True
        self.client.listed_customer = True

        cleaned = MODULE.cleanup_run(self.run_dir, value, self.client)

        self.assertEqual(cleaned["status"], "cleaned")
        self.assertEqual(cleaned["clock"]["id"], "clock_test123")
        self.assertEqual(cleaned["customer"]["id"], "cus_test123")
        self.assertEqual(
            [call[1] for call in self.client.calls if call[0] == "DELETE"],
            ["/v1/customers/cus_test123", "/v1/test_helpers/test_clocks/clock_test123"],
        )

    def test_cleanup_does_not_claim_success_when_uncertain_create_is_not_listed(self):
        value = manifest()
        value["clock"]["create_attempted"] = True
        MODULE.private_atomic_json(self.run_dir / "run.json", value)

        with self.assertRaisesRegex(MODULE.RunError, "run_resource_create_unresolved"):
            MODULE.cleanup_run(self.run_dir, value, self.client)

        saved = MODULE.read_manifest(self.run_dir)
        self.assertEqual(saved["status"], "cleanup_incomplete")
        self.assertTrue(saved["clock"]["create_attempted"])
        self.assertFalse(any(call[0] == "DELETE" for call in self.client.calls))

    def test_uncertain_customer_create_can_resume_after_cleanup_finds_no_customer(self):
        value = manifest()
        MODULE.private_atomic_json(self.run_dir / "run.json", value)
        self.client.customer_create_error = "stripe_request_unavailable"

        with self.assertRaisesRegex(MODULE.RunError, "stripe_request_unavailable"):
            MODULE.resume_run(self.run_dir, value, self.client)
        with self.assertRaisesRegex(MODULE.RunError, "run_resource_create_unresolved"):
            MODULE.cleanup_run(self.run_dir, MODULE.read_manifest(self.run_dir), self.client)

        incomplete = MODULE.read_manifest(self.run_dir)
        self.assertEqual(incomplete["status"], "cleanup_incomplete")
        self.assertTrue(incomplete["customer"]["create_attempted"])
        self.assertIsNone(incomplete["customer"]["id"])
        resumed = MODULE.resume_run(self.run_dir, incomplete, self.client)
        self.assertEqual(resumed["status"], "prepared")
        cleaned = MODULE.cleanup_run(self.run_dir, resumed, self.client)

        self.assertEqual(cleaned["status"], "cleaned")
        self.assertTrue(cleaned["customer"]["deleted"])
        self.assertTrue(cleaned["clock"]["deleted"])

    def test_cleanup_resumes_after_customer_delete_response_is_lost(self):
        value = manifest()
        value["clock"] = {
            "id": "clock_test123", "created": True, "deleted": False,
            "create_attempted": False,
        }
        value["customer"] = {
            "id": "cus_test123", "created": True, "deleted": False,
            "create_attempted": False,
        }
        MODULE.private_atomic_json(self.run_dir / "run.json", value)
        self.client.fail_customer_delete_after_apply = True

        with self.assertRaisesRegex(MODULE.RunError, "stripe_request_unavailable"):
            MODULE.cleanup_run(self.run_dir, value, self.client)
        cleaned = MODULE.cleanup_run(self.run_dir, MODULE.read_manifest(self.run_dir), self.client)

        self.assertEqual(cleaned["status"], "cleaned")
        self.assertTrue(cleaned["customer"]["deleted"])
        self.assertTrue(cleaned["clock"]["deleted"])

    def test_cleanup_resumes_after_clock_delete_response_is_lost(self):
        value = manifest()
        value["status"] = "prepared"
        value["clock"] = {
            "id": "clock_test123", "created": True, "deleted": False,
            "create_attempted": False,
        }
        value["customer"] = {
            "id": "cus_test123", "created": True, "deleted": False,
            "create_attempted": False,
        }
        MODULE.private_atomic_json(self.run_dir / "run.json", value)
        self.client.fail_clock_delete_after_apply = True

        with self.assertRaisesRegex(MODULE.RunError, "stripe_request_unavailable"):
            MODULE.cleanup_run(self.run_dir, value, self.client)
        interrupted = MODULE.read_manifest(self.run_dir)
        self.assertEqual(interrupted["status"], "cleanup_incomplete")
        self.assertTrue(interrupted["customer"]["deleted"])
        with self.assertRaisesRegex(MODULE.RunError, "run_is_not_resumable"):
            MODULE.resume_run(self.run_dir, interrupted, self.client)
        cleaned = MODULE.cleanup_run(self.run_dir, interrupted, self.client)

        self.assertEqual(cleaned["status"], "cleaned")
        self.assertTrue(cleaned["clock"]["deleted"])

    def test_uncertain_customer_create_conflict_cannot_report_cleanup_success(self):
        value = manifest()
        value["clock"] = {
            "id": "clock_test123", "created": True, "deleted": False,
            "create_attempted": False,
        }
        MODULE.private_atomic_json(self.run_dir / "run.json", value)
        self.client.customer_create_error = "stripe_request_uncertain"

        with self.assertRaisesRegex(MODULE.RunError, "stripe_request_uncertain"):
            MODULE.resume_run(self.run_dir, value, self.client)
        with self.assertRaisesRegex(MODULE.RunError, "run_resource_create_unresolved"):
            MODULE.cleanup_run(self.run_dir, MODULE.read_manifest(self.run_dir), self.client)

        self.client.listed_customer = True
        cleaned = MODULE.cleanup_run(self.run_dir, MODULE.read_manifest(self.run_dir), self.client)
        self.assertEqual(cleaned["status"], "cleaned")

    def test_cleanup_refuses_ambiguous_run_resources_before_deleting(self):
        value = manifest()
        value["clock"]["create_attempted"] = True
        self.client.listed_clock = True
        self.client.duplicate_clock = True

        with self.assertRaisesRegex(MODULE.RunError, "test_clock_run_id_ambiguous"):
            MODULE.cleanup_run(self.run_dir, value, self.client)
        self.assertFalse(any(call[0] == "DELETE" for call in self.client.calls))

    def test_clock_recovery_paginates_with_bounded_cursor_requests(self):
        first = {
            "object": "list", "has_more": True,
            "data": [{
                "id": "clock_other", "object": "test_helpers.test_clock",
                "livemode": False, "name": "other-run",
            }],
        }
        second = {
            "object": "list", "has_more": False,
            "data": [{
                "id": "clock_test123", "object": "test_helpers.test_clock",
                "livemode": False,
                "name": "sotto-lifecycle-11111111-1111-4111-8111-111111111111",
            }],
        }
        request = mock.Mock(side_effect=[first, second])
        self.client.request = request

        clock = MODULE.find_run_clock(
            self.client,
            "sotto-lifecycle-11111111-1111-4111-8111-111111111111",
        )

        self.assertEqual(clock["id"], "clock_test123")
        self.assertEqual(request.call_args_list[0].args, ("GET", "/v1/test_helpers/test_clocks", {"limit": 100}))
        self.assertEqual(request.call_args_list[1].args[2], {"limit": 100, "starting_after": "clock_other"})

    def test_clock_recovery_stops_at_page_bound(self):
        page = {
            "object": "list", "has_more": True,
            "data": [{
                "id": "clock_page", "object": "test_helpers.test_clock",
                "livemode": False, "name": "other-run",
            }],
        }
        request = mock.Mock(return_value=page)
        self.client.request = request

        with self.assertRaisesRegex(MODULE.RunError, "test_clock_list_bound_exceeded"):
            MODULE.find_run_clock(self.client, "missing-run")
        self.assertEqual(request.call_count, MODULE.RESOURCE_RECOVERY_PAGE_LIMIT)

    def test_customer_recovery_rejects_ambiguous_run_identity(self):
        value = manifest()
        value["clock"] = {
            "id": "clock_test123", "created": True, "deleted": False,
            "create_attempted": False,
        }
        value["customer"]["create_attempted"] = True
        self.client.listed_customer = True
        self.client.duplicate_customer = True

        with self.assertRaisesRegex(MODULE.RunError, "customer_run_id_ambiguous"):
            MODULE.cleanup_run(self.run_dir, value, self.client)

        self.assertFalse(any(call[0] == "DELETE" for call in self.client.calls))

    def test_customer_recovery_stops_at_page_bound(self):
        page = {
            "object": "list", "has_more": True,
            "data": [{
                "id": "cus_other", "object": "customer", "livemode": False,
                "test_clock": "clock_test123", "metadata": {},
            }],
        }
        request = mock.Mock(return_value=page)
        self.client.request = request

        with self.assertRaisesRegex(MODULE.RunError, "customer_list_bound_exceeded"):
            MODULE.find_run_customer(self.client, "clock_test123", "missing-run")

        self.assertEqual(request.call_count, MODULE.RESOURCE_RECOVERY_PAGE_LIMIT)

    def test_stripe_client_accepts_list_envelopes_without_top_level_livemode(self):
        client = MODULE.StripeClient("rk_test_not-a-real-key", "acct_123")
        response = mock.MagicMock()
        response.__enter__.return_value = response
        response.read.return_value = json.dumps({
            "object": "list", "data": [], "has_more": False,
        }).encode("utf-8")
        with mock.patch.object(client._opener, "open", return_value=response):
            value = client.request("GET", "/v1/customers", {"test_clock": "clock_test123"})
        self.assertEqual(value["object"], "list")

    def test_stripe_client_accepts_deleted_customer_tombstone_without_livemode(self):
        client = MODULE.StripeClient("rk_test_not-a-real-key", "acct_123")
        response = mock.MagicMock()
        response.__enter__.return_value = response
        response.read.return_value = json.dumps({
            "id": "cus_test123", "object": "customer", "deleted": True,
        }).encode("utf-8")
        with mock.patch.object(client._opener, "open", return_value=response):
            value = client.request("GET", "/v1/customers/cus_test123")
        self.assertIs(value["deleted"], True)

    def test_stripe_client_keeps_idempotency_conflicts_uncertain(self):
        client = MODULE.StripeClient("rk_test_not-a-real-key", "acct_123")
        conflict = MODULE.HTTPError(
            "https://api.stripe.com/v1/customers", 409, "conflict", {}, io.BytesIO(b"private"),
        )
        with mock.patch.object(client._opener, "open", side_effect=conflict):
            with self.assertRaisesRegex(MODULE.RunError, "stripe_request_uncertain"):
                client.request(
                    "POST", "/v1/customers", {"name": "test"},
                    idempotency_key="sotto-lifecycle:run:customer",
                )

    def test_stripe_client_keeps_gateway_timeouts_uncertain(self):
        client = MODULE.StripeClient("rk_test_not-a-real-key", "acct_123")
        timeout = MODULE.HTTPError(
            "https://api.stripe.com/v1/customers", 408, "request timeout", {}, io.BytesIO(b"private"),
        )
        with mock.patch.object(client._opener, "open", side_effect=timeout):
            with self.assertRaisesRegex(MODULE.RunError, "stripe_request_uncertain"):
                client.request(
                    "POST", "/v1/customers", {"name": "test"},
                    idempotency_key="sotto-lifecycle:run:customer",
                )

    def test_cleanup_resumes_through_http_client_after_lost_customer_delete_response(self):
        value = manifest()
        value["status"] = "prepared"
        value["clock"] = {
            "id": "clock_test123", "created": True, "deleted": False,
            "create_attempted": False,
        }
        value["customer"] = {
            "id": "cus_test123", "created": True, "deleted": False,
            "create_attempted": False,
        }
        MODULE.private_atomic_json(self.run_dir / "run.json", value)
        client = MODULE.StripeClient("rk_test_not-a-real-key", "acct_123")
        state = {"customer_deleted": False}

        def response(payload):
            result = mock.MagicMock()
            result.__enter__.return_value = result
            result.read.return_value = json.dumps(payload).encode("utf-8")
            return result

        def stripe_open(request, timeout):
            path = request.full_url.removeprefix("https://api.stripe.com")
            if path == "/v1/customers/cus_test123" and request.get_method() == "GET":
                if state["customer_deleted"]:
                    return response({
                        "id": "cus_test123", "object": "customer", "deleted": True,
                    })
                return response({
                    "id": "cus_test123", "object": "customer", "livemode": False,
                    "test_clock": "clock_test123",
                    "metadata": {"sotto_lifecycle_run_id": value["run_id"]},
                })
            if path in {
                "/v1/customers/cus_test123/payment_methods?limit=1",
                "/v1/customers/cus_test123/sources?limit=1",
                "/v1/subscriptions?customer=cus_test123&status=all&limit=1",
            }:
                return response({"object": "list", "data": [], "has_more": False})
            if path == "/v1/customers/cus_test123" and request.get_method() == "DELETE":
                state["customer_deleted"] = True
                raise MODULE.URLError("response lost after provider deletion")
            if (
                path == "/v1/test_helpers/test_clocks/clock_test123"
                and request.get_method() == "GET"
            ):
                return response({
                    "id": "clock_test123", "object": "test_helpers.test_clock",
                    "livemode": False,
                    "name": f"sotto-lifecycle-{value['run_id']}", "status": "ready",
                })
            if (
                path == "/v1/test_helpers/test_clocks/clock_test123"
                and request.get_method() == "DELETE"
            ):
                return response({
                    "id": "clock_test123", "object": "test_helpers.test_clock", "deleted": True,
                })
            raise AssertionError((request.get_method(), path, timeout))

        with mock.patch.object(client._opener, "open", side_effect=stripe_open):
            with self.assertRaisesRegex(MODULE.RunError, "stripe_request_unavailable"):
                MODULE.cleanup_run(self.run_dir, value, client)
            cleaned = MODULE.cleanup_run(self.run_dir, MODULE.read_manifest(self.run_dir), client)

        self.assertEqual(cleaned["status"], "cleaned")
        self.assertTrue(cleaned["customer"]["deleted"])
        self.assertTrue(cleaned["clock"]["deleted"])

    def test_cleanup_checks_all_saved_payment_method_types_via_stripe_client(self):
        value = manifest()
        value["clock"] = {
            "id": "clock_test123", "created": True, "deleted": False,
            "create_attempted": False,
        }
        value["customer"] = {
            "id": "cus_test123", "created": True, "deleted": False,
            "create_attempted": False,
        }
        client = MODULE.StripeClient("rk_test_not-a-real-key", "acct_123")
        requests = []

        def response(payload):
            result = mock.MagicMock()
            result.__enter__.return_value = result
            result.read.return_value = json.dumps(payload).encode("utf-8")
            return result

        def stripe_open(request, timeout):
            requests.append(request)
            path = request.full_url.removeprefix("https://api.stripe.com")
            if path == "/v1/customers/cus_test123":
                return response({
                    "id": "cus_test123", "object": "customer", "livemode": False,
                    "test_clock": "clock_test123",
                    "metadata": {"sotto_lifecycle_run_id": value["run_id"]},
                })
            if path == "/v1/customers/cus_test123/payment_methods?limit=1":
                return response({
                    "object": "list", "data": [{"id": "pm_saved"}], "has_more": False,
                })
            raise AssertionError(path)

        with mock.patch.object(client._opener, "open", side_effect=stripe_open):
            with self.assertRaisesRegex(MODULE.RunError, "customer_has_saved_payment_instruments"):
                MODULE.cleanup_run(self.run_dir, value, client)

        self.assertEqual(len(requests), 2)
        self.assertEqual(
            requests[1].full_url,
            "https://api.stripe.com/v1/customers/cus_test123/payment_methods?limit=1",
        )
        self.assertFalse(any(request.get_method() == "DELETE" for request in requests))

    def test_resume_refuses_after_safe_idempotency_horizon(self):
        value = manifest()
        value["started_at_epoch"] = int(time.time()) - MODULE.RESUME_HORIZON_SECONDS - 1
        with self.assertRaisesRegex(MODULE.RunError, "resume_idempotency_horizon_exceeded"):
            MODULE.resume_run(self.run_dir, value, self.client)
        self.assertEqual(self.client.calls, [])

    def test_cleanup_incomplete_uncertain_create_cannot_replay_past_key_horizon(self):
        value = manifest()
        value["status"] = "cleanup_incomplete"
        value["started_at_epoch"] = int(time.time()) - MODULE.RESUME_HORIZON_SECONDS - 1
        value["started_at"] = MODULE.datetime.fromtimestamp(
            value["started_at_epoch"], MODULE.timezone.utc,
        ).isoformat().replace("+00:00", "Z")
        value["frozen_time"] = value["started_at_epoch"] - 300
        value["clock"].update(id="clock_test123", created=True)
        value["customer"]["create_attempted"] = True

        with self.assertRaisesRegex(MODULE.RunError, "resume_idempotency_horizon_exceeded"):
            MODULE.resume_run(self.run_dir, value, self.client)

        self.assertEqual(self.client.calls, [])

    def test_prepare_requires_clock_time_old_enough_for_preflight_reads(self):
        with mock.patch.object(MODULE.PREFLIGHT, "run_preflight") as preflight:
            with self.assertRaisesRegex(MODULE.RunError, "test_clock_frozen_time_out_of_range"):
                MODULE.make_run(
                    self.run_dir, int(time.time()) - 30, self.client, {}, "acct_123", "a" * 40
                )
        preflight.assert_not_called()
        self.assertEqual(self.client.calls, [])

    def test_resume_rejects_clock_with_a_different_frozen_time(self):
        value = manifest()
        self.client.clock_frozen_time_offset = 1

        with self.assertRaisesRegex(MODULE.RunError, "test_clock_response_mismatch"):
            MODULE.resume_run(self.run_dir, value, self.client)

        saved = MODULE.read_manifest(self.run_dir)
        self.assertTrue(value["clock"]["create_attempted"])
        self.assertTrue(saved["clock"]["create_attempted"])

    def test_resume_does_not_report_prepared_when_saved_resources_no_longer_match(self):
        value = manifest()
        value["status"] = "prepared"
        value["clock"].update(id="clock_test123", created=True)
        value["customer"].update(id="cus_test123", created=True)
        MODULE.private_atomic_json(self.run_dir / "run.json", value)
        self.client.customer_deleted = True

        with self.assertRaisesRegex(MODULE.RunError, "customer_response_mismatch"):
            MODULE.resume_run(self.run_dir, MODULE.read_manifest(self.run_dir), self.client)

        self.assertEqual(self.client.calls, [
            ("GET", "/v1/test_helpers/test_clocks/clock_test123", None, None),
            ("GET", "/v1/customers/cus_test123", None, None),
        ])
        self.assertEqual(MODULE.read_manifest(self.run_dir)["status"], "prepared")

    def test_resume_checks_existing_clock_before_creating_missing_customer(self):
        value = manifest()
        value["clock"].update(id="clock_test123", created=True)
        self.client.read_clock_frozen_time_offset = -1

        with self.assertRaisesRegex(MODULE.RunError, "test_clock_response_mismatch"):
            MODULE.resume_run(self.run_dir, value, self.client)

        self.assertFalse(any(call[1] == "/v1/customers" for call in self.client.calls))

    def test_resume_accepts_an_owned_clock_advanced_by_a_lifecycle_run(self):
        value = manifest()
        value["status"] = "prepared"
        value["clock"].update(id="clock_test123", created=True)
        value["customer"].update(id="cus_test123", created=True)
        self.client.created_clock_frozen_time = value["frozen_time"]
        self.client.read_clock_frozen_time_offset = 86_400

        resumed = MODULE.resume_run(self.run_dir, value, self.client)

        self.assertEqual(resumed["status"], "prepared")
        self.assertEqual([call[1] for call in self.client.calls], [
            "/v1/test_helpers/test_clocks/clock_test123", "/v1/customers/cus_test123",
        ])

    def test_cli_prepare_resume_and_cleanup_complete_against_fake_provider(self):
        fake = self.client
        with (
            mock.patch.object(MODULE, "key_and_config", return_value=(
                "rk_test_fake", {}, "acct_123", "a" * 40,
            )),
            mock.patch.object(MODULE, "key_and_identity", return_value=("rk_test_fake", "acct_123")),
            mock.patch.object(MODULE, "StripeClient", return_value=fake),
            mock.patch.object(MODULE.PREFLIGHT, "run_preflight"),
        ):
            prepared = MODULE.main([
                "prepare", "--run-dir", str(self.run_dir),
                "--frozen-time", str(int(time.time()) - 300),
            ])
            resumed = MODULE.main(["resume", "--run-dir", str(self.run_dir)])
            cleaned = MODULE.main(["cleanup", "--run-dir", str(self.run_dir)])

        self.assertEqual((prepared, resumed, cleaned), (0, 0, 0))
        self.assertEqual(MODULE.read_manifest(self.run_dir)["status"], "cleaned")

    def test_manifest_rejects_public_permissions_and_wrong_account(self):
        value = manifest()
        MODULE.private_atomic_json(self.run_dir / "run.json", value)
        os.chmod(self.run_dir / "run.json", 0o644)
        with self.assertRaisesRegex(MODULE.RunError, "manifest_permissions_invalid"):
            MODULE.read_manifest(self.run_dir)

    def test_manifest_rejects_wrong_field_types_without_leaking_python_errors(self):
        mutations = (
            ("api_version", []),
            ("app_sha", 7),
            ("account_fingerprint", {}),
            ("started_at", 123),
            ("status", []),
        )
        for field, replacement in mutations:
            with self.subTest(field=field):
                value = manifest()
                value[field] = replacement
                MODULE.private_atomic_json(self.run_dir / "run.json", value)
                with self.assertRaises(MODULE.RunError):
                    MODULE.read_manifest(self.run_dir)

    def test_manifest_status_requires_consistent_resource_lifecycle(self):
        mutations = (
            ("prepared_deleted", "prepared", "clock", "deleted", True),
            ("cleaned_create_unknown", "cleaned", "clock", "create_attempted", True),
        )
        for name, status, resource, field, replacement in mutations:
            with self.subTest(name=name):
                value = manifest()
                value["status"] = status
                if status == "prepared":
                    for item in ("clock", "customer"):
                        value[item].update(id=f"{'clock' if item == 'clock' else 'cus'}_test123", created=True)
                value[resource][field] = replacement
                MODULE.private_atomic_json(self.run_dir / "run.json", value)
                with self.assertRaisesRegex(MODULE.RunError, "manifest_status_resource_mismatch"):
                    MODULE.read_manifest(self.run_dir)

        value = manifest()
        value["status"] = "prepared"
        value["clock"].update(id="clock_test123", created=True)
        MODULE.private_atomic_json(self.run_dir / "run.json", value)
        with self.assertRaisesRegex(MODULE.RunError, "manifest_status_resource_mismatch"):
            MODULE.read_manifest(self.run_dir)

    def test_run_lock_serialises_prepare_resume_and_cleanup_commands(self):
        with MODULE.lock_run(self.run_dir):
            with self.assertRaisesRegex(MODULE.RunError, "run_already_locked"):
                with MODULE.lock_run(self.run_dir):
                    self.fail("second command acquired the run lock")

    def test_client_rejects_unlisted_endpoint_before_network_access(self):
        client = MODULE.StripeClient("rk_test_not-a-real-key", "acct_123")
        with self.assertRaisesRegex(MODULE.RunError, "stripe_endpoint_not_allowed"):
            client.request("POST", "/v1/refunds")


if __name__ == "__main__":
    unittest.main()
