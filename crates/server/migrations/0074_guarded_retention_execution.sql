-- A purge approval is bound to a reviewed dry-run snapshot and an independently authenticated
-- restore rehearsal. The worker must revalidate both before every bounded batch.

ALTER TABLE cloud_retention_jobs
    ADD COLUMN execution_scope_generation BIGINT
        CHECK (execution_scope_generation IS NULL OR execution_scope_generation >= 0);

ALTER TABLE cloud_retention_tombstone_journal
    DROP CONSTRAINT cloud_retention_tombstone_journal_resource_kind_check,
    ADD CONSTRAINT cloud_retention_tombstone_journal_resource_kind_check
        CHECK (resource_kind IN ('project', 'environment', 'grant', 'machine_token', 'share'));

CREATE TABLE cloud_retention_restore_evidence (
    evidence_id                 TEXT PRIMARY KEY,
    release_sha                 TEXT NOT NULL CHECK (release_sha ~ '^[0-9a-f]{40}([0-9a-f]{24})?$'),
    backup_fingerprint          TEXT NOT NULL CHECK (backup_fingerprint ~ '^[0-9a-f]{64}$'),
    sidecar_sha256              TEXT NOT NULL CHECK (sidecar_sha256 ~ '^[0-9a-f]{64}$'),
    journal_entry_count         BIGINT NOT NULL CHECK (journal_entry_count >= 0),
    replay_entry_count          BIGINT NOT NULL CHECK (replay_entry_count >= 0),
    first_replay_deleted_count  BIGINT NOT NULL CHECK (first_replay_deleted_count >= 0),
    second_replay_deleted_count BIGINT NOT NULL CHECK (second_replay_deleted_count = 0),
    incarnation_guard_verified  BOOLEAN NOT NULL CHECK (incarnation_guard_verified),
    signature                   TEXT NOT NULL CHECK (signature ~ '^[0-9a-f]{64}$'),
    recorded_by                 TEXT NOT NULL CHECK (length(btrim(recorded_by)) BETWEEN 1 AND 80),
    recorded_at                 TIMESTAMPTZ NOT NULL DEFAULT now(),
    CHECK (replay_entry_count = journal_entry_count),
    CHECK (first_replay_deleted_count <= replay_entry_count)
);

CREATE TABLE cloud_retention_purge_approvals (
    approval_id          TEXT PRIMARY KEY,
    job_id               TEXT NOT NULL REFERENCES cloud_retention_jobs (job_id) ON DELETE RESTRICT,
    scope_fingerprint    TEXT NOT NULL CHECK (scope_fingerprint ~ '^[0-9a-f]{64}$'),
    policy_version       INTEGER NOT NULL CHECK (policy_version > 0),
    evidence_id          TEXT NOT NULL REFERENCES cloud_retention_restore_evidence (evidence_id) ON DELETE RESTRICT,
    journal_entry_count  BIGINT NOT NULL CHECK (journal_entry_count >= 0),
    actor_label          TEXT NOT NULL CHECK (length(btrim(actor_label)) BETWEEN 1 AND 80),
    state                TEXT NOT NULL DEFAULT 'active'
        CHECK (state IN ('active', 'stale', 'revoked', 'completed')),
    approved_at          TIMESTAMPTZ NOT NULL DEFAULT now(),
    invalidated_at       TIMESTAMPTZ
);

CREATE UNIQUE INDEX cloud_retention_purge_approvals_active_job_idx
    ON cloud_retention_purge_approvals (job_id) WHERE state = 'active';

CREATE INDEX cloud_retention_purge_approvals_job_idx
    ON cloud_retention_purge_approvals (job_id, approved_at DESC, approval_id);

CREATE INDEX cloud_retention_jobs_purge_due_idx
    ON cloud_retention_jobs (deadline_at, job_id)
    WHERE state IN ('ready', 'leased') AND purge_enabled = TRUE AND dry_run = FALSE;

ALTER TABLE cloud_retention_tombstone_journal
    ADD CONSTRAINT cloud_retention_tombstone_journal_owner_required_check
        CHECK (expected_owner_id IS NOT NULL);
