-- Organisation retention is driven by a complete, generation-fenced snapshot. The first
-- implementation only prepares and dry-runs shared scopes; it never grants purge authority.

ALTER TABLE organizations
    ADD COLUMN retention_generation BIGINT NOT NULL DEFAULT 0
        CHECK (retention_generation >= 0);

ALTER TABLE cloud_retention_jobs
    ADD COLUMN expected_scope_generation BIGINT
        CHECK (expected_scope_generation IS NULL OR expected_scope_generation >= 0),
    ADD COLUMN eligibility_loss_at TIMESTAMPTZ,
    ADD CONSTRAINT cloud_retention_jobs_scope_generation_check CHECK (
        (scope_kind = 'personal' AND expected_scope_generation IS NULL)
        OR (scope_kind IN ('shared_organisation', 'orphan_organisation')
            AND expected_scope_generation IS NOT NULL)
    ),
    ADD CONSTRAINT cloud_retention_jobs_eligibility_loss_check CHECK (
        (scope_kind = 'shared_organisation' AND
            (eligibility_episode NOT LIKE 'shared-export:%' OR eligibility_loss_at IS NOT NULL))
        OR (scope_kind <> 'shared_organisation' AND eligibility_loss_at IS NULL)
    );

ALTER TABLE cloud_retention_scope_items
    DROP CONSTRAINT cloud_retention_scope_items_resource_kind_check,
    ADD CONSTRAINT cloud_retention_scope_items_resource_kind_check
        CHECK (resource_kind IN ('project', 'environment', 'grant', 'machine_token', 'share'));

ALTER TABLE cloud_retention_scan_state
    DROP CONSTRAINT cloud_retention_scan_state_scan_key_check,
    ADD CONSTRAINT cloud_retention_scan_state_scan_key_check
        CHECK (scan_key IN ('personal_expiry', 'shared_organisation_retention')),
    ADD COLUMN cursor_scope_id TEXT;

INSERT INTO cloud_retention_scan_state(scan_key)
VALUES ('shared_organisation_retention')
ON CONFLICT (scan_key) DO NOTHING;

CREATE FUNCTION bump_organisation_retention_generation(target_org_id TEXT) RETURNS VOID AS $$
BEGIN
    IF target_org_id IS NOT NULL THEN
        UPDATE organizations SET retention_generation = retention_generation + 1
        WHERE id = target_org_id;
    END IF;
END;
$$ LANGUAGE plpgsql;

CREATE FUNCTION track_organisation_membership_retention_generation() RETURNS TRIGGER AS $$
BEGIN
    IF TG_OP = 'INSERT' THEN
        PERFORM bump_organisation_retention_generation(NEW.org_id);
        RETURN NEW;
    ELSIF TG_OP = 'DELETE' THEN
        PERFORM bump_organisation_retention_generation(OLD.org_id);
        RETURN OLD;
    END IF;
    IF OLD.org_id IS DISTINCT FROM NEW.org_id THEN
        PERFORM bump_organisation_retention_generation(OLD.org_id);
        PERFORM bump_organisation_retention_generation(NEW.org_id);
    ELSE
        PERFORM bump_organisation_retention_generation(NEW.org_id);
    END IF;
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER organisation_membership_retention_generation
AFTER INSERT OR UPDATE OR DELETE ON organization_memberships
FOR EACH ROW EXECUTE FUNCTION track_organisation_membership_retention_generation();

CREATE FUNCTION track_project_retention_generation() RETURNS TRIGGER AS $$
BEGIN
    IF TG_OP = 'INSERT' THEN
        PERFORM bump_organisation_retention_generation(NEW.org_id);
        RETURN NEW;
    ELSIF TG_OP = 'DELETE' THEN
        PERFORM bump_organisation_retention_generation(OLD.org_id);
        RETURN OLD;
    END IF;
    IF OLD.org_id IS DISTINCT FROM NEW.org_id THEN
        PERFORM bump_organisation_retention_generation(OLD.org_id);
        PERFORM bump_organisation_retention_generation(NEW.org_id);
    ELSE
        PERFORM bump_organisation_retention_generation(NEW.org_id);
    END IF;
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER project_retention_generation
AFTER INSERT OR UPDATE OR DELETE ON projects
FOR EACH ROW EXECUTE FUNCTION track_project_retention_generation();

CREATE FUNCTION track_environment_retention_generation() RETURNS TRIGGER AS $$
DECLARE
    old_org_id TEXT;
    new_org_id TEXT;
BEGIN
    IF TG_OP <> 'INSERT' THEN
        SELECT org_id INTO old_org_id FROM projects WHERE id = OLD.project_id;
    END IF;
    IF TG_OP <> 'DELETE' THEN
        SELECT org_id INTO new_org_id FROM projects WHERE id = NEW.project_id;
    END IF;
    IF old_org_id IS DISTINCT FROM new_org_id THEN
        PERFORM bump_organisation_retention_generation(old_org_id);
    END IF;
    PERFORM bump_organisation_retention_generation(new_org_id);
    IF TG_OP = 'DELETE' THEN RETURN OLD; END IF;
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER environment_retention_generation
AFTER INSERT OR UPDATE OR DELETE ON environments
FOR EACH ROW EXECUTE FUNCTION track_environment_retention_generation();

CREATE FUNCTION track_environment_grant_retention_generation() RETURNS TRIGGER AS $$
DECLARE
    old_org_id TEXT;
    new_org_id TEXT;
BEGIN
    IF TG_OP <> 'INSERT' THEN
        SELECT p.org_id INTO old_org_id
        FROM environments e JOIN projects p ON p.id=e.project_id
        WHERE e.id=OLD.env_id;
    END IF;
    IF TG_OP <> 'DELETE' THEN
        SELECT p.org_id INTO new_org_id
        FROM environments e JOIN projects p ON p.id=e.project_id
        WHERE e.id=NEW.env_id;
    END IF;
    IF old_org_id IS DISTINCT FROM new_org_id THEN
        PERFORM bump_organisation_retention_generation(old_org_id);
    END IF;
    PERFORM bump_organisation_retention_generation(new_org_id);
    IF TG_OP = 'DELETE' THEN RETURN OLD; END IF;
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER environment_grant_retention_generation
AFTER INSERT OR UPDATE OR DELETE ON environment_grants
FOR EACH ROW EXECUTE FUNCTION track_environment_grant_retention_generation();

CREATE FUNCTION track_machine_token_retention_generation() RETURNS TRIGGER AS $$
DECLARE
    old_org_id TEXT;
    new_org_id TEXT;
BEGIN
    IF TG_OP <> 'INSERT' THEN
        SELECT p.org_id INTO old_org_id
        FROM environments e JOIN projects p ON p.id=e.project_id
        WHERE e.id=OLD.env_id;
    END IF;
    IF TG_OP <> 'DELETE' THEN
        SELECT p.org_id INTO new_org_id
        FROM environments e JOIN projects p ON p.id=e.project_id
        WHERE e.id=NEW.env_id;
    END IF;
    IF old_org_id IS DISTINCT FROM new_org_id THEN
        PERFORM bump_organisation_retention_generation(old_org_id);
    END IF;
    PERFORM bump_organisation_retention_generation(new_org_id);
    IF TG_OP = 'DELETE' THEN RETURN OLD; END IF;
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER machine_token_retention_generation
AFTER INSERT OR UPDATE OR DELETE ON machine_tokens
FOR EACH ROW EXECUTE FUNCTION track_machine_token_retention_generation();

CREATE INDEX cloud_retention_organisation_generation_idx
    ON cloud_retention_jobs (organisation_id, expected_scope_generation)
    WHERE scope_kind IN ('shared_organisation', 'orphan_organisation');
