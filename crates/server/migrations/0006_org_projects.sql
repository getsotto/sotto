-- Org-owned projects (M5 PR3a): a project may belong to an organization.
--
-- `org_id IS NULL`  → personal project: access is governed by `owner_id` (the pre-M5 behavior,
--                     preserved for every existing row, which backfills to NULL).
-- `org_id` set      → org project: access is governed by the caller's membership role in that org
--                     (reads + secret writes for any member; structural changes for admin+).
--
-- `owner_id` is retained as the creator/provenance either way. Deleting the org cascades its
-- projects (and, through the existing FKs, their environments and secrets) — deleting an org is a
-- deliberate, owner-only action.

ALTER TABLE projects
    ADD COLUMN IF NOT EXISTS org_id TEXT REFERENCES organizations (id) ON DELETE CASCADE;

CREATE INDEX IF NOT EXISTS projects_org_id_idx ON projects (org_id);
