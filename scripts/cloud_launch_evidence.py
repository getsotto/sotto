"""Core validation for the privacy-preserving Cloud launch evidence index."""

from __future__ import annotations

import hashlib
import json
import re
from datetime import datetime
from pathlib import Path

SCHEMA = "sotto-cloud-launch-evidence-v1"
PINNED_API_VERSION = "2026-07-29.dahlia"
SHA_RE = re.compile(r"^[0-9a-f]{40}$")
SHA256_RE = re.compile(r"^[0-9a-f]{64}$")
RFC3339_RE = re.compile(r"^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d+)?(?:Z|[+-]\d{2}:\d{2})$")

REQUIRED_REQUIREMENTS = tuple(f"C{i:02}" for i in range(1, 15))
REQUIRED_DECISIONS = tuple(f"D{i:02}" for i in range(1, 13))
REQUIRED_CHECKS = (
    "ci",
    "database",
    "native_wasm_crypto",
    "previous_minor_clients",
    "stripe_lifecycle_17_scenarios",
    "restore_rehearsal",
    "production_smoke",
    "incident_drill",
)


def fingerprint(value: str) -> str:
    return hashlib.sha256(value.encode("utf-8")).hexdigest()[:16]


def _unique_object(pairs):
    result = {}
    for key, value in pairs:
        if key in result:
            raise ValueError("Cloud launch evidence contains a duplicate JSON field")
        result[key] = value
    return result


def _require_digest(value: object, name: str) -> str:
    if not isinstance(value, str) or not SHA256_RE.fullmatch(value):
        raise ValueError(f"{name} must be a lowercase sha256 digest")
    return value


def _require_candidate_sha(value: object) -> str:
    if not isinstance(value, str) or not SHA_RE.fullmatch(value):
        raise ValueError("app_sha must be a full lowercase commit SHA")
    return value


def _validate_timestamp(value: object) -> str:
    if not isinstance(value, str) or not RFC3339_RE.fullmatch(value):
        raise ValueError("captured_at must be an RFC3339 timestamp")
    try:
        parsed = datetime.fromisoformat(value.replace("Z", "+00:00"))
    except ValueError as error:
        raise ValueError("captured_at must be an RFC3339 timestamp") from error
    if parsed.tzinfo is None or parsed.utcoffset() is None:
        raise ValueError("captured_at must include a timezone")
    return value


def _validate_rows(value, *, field, expected_ids, id_field, allowed_results, candidate_sha, evidence_field):
    rows = value[field]
    if not isinstance(rows, list) or len(rows) != len(expected_ids):
        raise ValueError(f"{field} must contain exactly one row for every required item")

    expected = set(expected_ids)
    seen = set()
    sanitized = []
    failures = []
    fingerprints = []
    for row in rows:
        if not isinstance(row, dict):
            raise ValueError(f"each {field} row must be an object")
        expected_keys = {id_field, "result", "tested_sha", evidence_field} if field != "decisions" else {
            id_field, "state", evidence_field
        }
        if set(row) != expected_keys:
            raise ValueError(f"{field} row has an incomplete shape")

        item_id = row[id_field]
        if not isinstance(item_id, str) or item_id not in expected or item_id in seen:
            raise ValueError(f"{field} contains an unknown or duplicate item")
        seen.add(item_id)

        if field == "decisions":
            state = row["state"]
            if not isinstance(state, str) or state not in allowed_results:
                raise ValueError(f"invalid state for {item_id}")
            digest = _require_digest(row[evidence_field], evidence_field)
            if state != "resolved":
                failures.append(f"{item_id}: {state}")
            sanitized.append({"id": item_id, "state": state})
        else:
            result = row["result"]
            if not isinstance(result, str) or result not in allowed_results:
                raise ValueError(f"invalid result for {item_id}")
            tested_sha = _require_candidate_sha(row["tested_sha"])
            if tested_sha != candidate_sha:
                raise ValueError(f"tested_sha for {item_id} must match app_sha")
            digest = _require_digest(row[evidence_field], evidence_field)
            if result != "passed":
                failures.append(f"{item_id}: {result}")
            sanitized.append({"id": item_id, "result": result})
        fingerprints.append(f"{item_id}:{digest}")

    missing = expected - seen
    if missing:
        raise ValueError(f"{field} is missing required items: {', '.join(sorted(missing))}")
    return sanitized, failures, fingerprints


def validate(path: str | Path) -> tuple[dict, list[str]]:
    """Validate one launch evidence index and return a sanitised report and incomplete rows."""
    with Path(path).open(encoding="utf-8") as stream:
        value = json.load(stream, object_pairs_hook=_unique_object)
    if not isinstance(value, dict):
        raise ValueError("Cloud launch evidence must be an object")
    expected_keys = {
        "schema",
        "app_sha",
        "stripe_api_version",
        "captured_at",
        "requirements",
        "decisions",
        "checks",
    }
    if set(value) != expected_keys:
        raise ValueError("Cloud launch evidence has an incomplete shape")
    if value["schema"] != SCHEMA:
        raise ValueError("unsupported Cloud launch evidence schema")

    candidate_sha = _require_candidate_sha(value["app_sha"])
    if value["stripe_api_version"] != PINNED_API_VERSION:
        raise ValueError(f"stripe_api_version must be {PINNED_API_VERSION}")
    captured_at = _validate_timestamp(value["captured_at"])

    requirement_rows, requirement_failures, requirement_refs = _validate_rows(
        value,
        field="requirements",
        expected_ids=REQUIRED_REQUIREMENTS,
        id_field="id",
        allowed_results={"passed", "failed", "blocked", "not_run"},
        candidate_sha=candidate_sha,
        evidence_field="evidence_sha256",
    )
    decision_rows, decision_failures, decision_refs = _validate_rows(
        value,
        field="decisions",
        expected_ids=REQUIRED_DECISIONS,
        id_field="id",
        allowed_results={"resolved", "unresolved"},
        candidate_sha=candidate_sha,
        evidence_field="record_sha256",
    )
    check_rows, check_failures, check_refs = _validate_rows(
        value,
        field="checks",
        expected_ids=REQUIRED_CHECKS,
        id_field="check",
        allowed_results={"passed", "failed", "blocked", "not_run"},
        candidate_sha=candidate_sha,
        evidence_field="artifact_sha256",
    )

    failures = requirement_failures + decision_failures + check_failures
    all_refs = "\n".join(sorted(requirement_refs + decision_refs + check_refs))
    report = {
        "schema": SCHEMA,
        "status": "incomplete" if failures else "evidence_complete",
        "launch_authorised": False,
        "app_sha": candidate_sha,
        "stripe_api_version": PINNED_API_VERSION,
        "captured_at": captured_at,
        "requirement_count": len(requirement_rows),
        "decision_count": len(decision_rows),
        "check_count": len(check_rows),
        "evidence_fingerprint": fingerprint(all_refs),
        "requirements": sorted(requirement_rows, key=lambda row: row["id"]),
        "decisions": sorted(decision_rows, key=lambda row: row["id"]),
        "checks": sorted(check_rows, key=lambda row: row["id"]),
    }
    return report, failures
