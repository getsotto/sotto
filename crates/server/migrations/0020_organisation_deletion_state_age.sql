-- State age must reset whenever a failed operation resumes or a provider cycle changes phase;
-- request age would make a fresh cancelling_billing attempt look permanently overdue.
ALTER TABLE organization_deletions
    ADD COLUMN state_entered_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    ADD CONSTRAINT organization_deletions_state_entered_at_check
        CHECK (state_entered_at >= requested_at);

-- Keep state age correct for every worker and operator transition, including future paths.
CREATE OR REPLACE FUNCTION set_organization_deletion_state_entered_at()
RETURNS TRIGGER AS $$
BEGIN
    IF NEW.state IS DISTINCT FROM OLD.state THEN
        NEW.state_entered_at = clock_timestamp();
    END IF;
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

DROP TRIGGER IF EXISTS organization_deletions_state_entered_at ON organization_deletions;
CREATE TRIGGER organization_deletions_state_entered_at
BEFORE UPDATE ON organization_deletions
FOR EACH ROW
EXECUTE FUNCTION set_organization_deletion_state_entered_at();
