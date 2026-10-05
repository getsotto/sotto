-- Bind each machine credential to the human whose eligibility is accountable for its use.
--
-- Existing rows with a surviving creator have an auditable provenance path, so they can be
-- migrated as verified. Rows whose creator was removed already have no trustworthy person to
-- charge or gate; they remain explicitly ambiguous until an operator recreates the token.

ALTER TABLE machine_tokens
    ADD COLUMN IF NOT EXISTS beneficiary_id TEXT REFERENCES users (id) ON DELETE SET NULL;

ALTER TABLE machine_tokens
    ADD COLUMN IF NOT EXISTS beneficiary_status TEXT NOT NULL DEFAULT 'ambiguous';

DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1
        FROM pg_constraint
        WHERE conrelid = 'machine_tokens'::regclass
          AND conname = 'machine_tokens_beneficiary_status'
    ) THEN
        ALTER TABLE machine_tokens
            ADD CONSTRAINT machine_tokens_beneficiary_status
            CHECK (beneficiary_status IN ('verified', 'ambiguous'));
    END IF;
END $$;

UPDATE machine_tokens
SET beneficiary_id = created_by,
    beneficiary_status = 'verified'
WHERE created_by IS NOT NULL
  AND beneficiary_id IS NULL;

CREATE INDEX IF NOT EXISTS machine_tokens_beneficiary_idx ON machine_tokens (beneficiary_id);

-- The foreign key clears beneficiary_id when its user is deleted. Keep the status honest so an
-- operator never sees a token as verified after its accountable person has disappeared.
CREATE OR REPLACE FUNCTION mark_machine_token_beneficiary_ambiguous()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF OLD.beneficiary_id IS NOT NULL AND NEW.beneficiary_id IS NULL THEN
        NEW.beneficiary_status := 'ambiguous';
    END IF;
    RETURN NEW;
END;
$$;

DROP TRIGGER IF EXISTS machine_tokens_beneficiary_status_on_clear ON machine_tokens;
CREATE TRIGGER machine_tokens_beneficiary_status_on_clear
BEFORE UPDATE OF beneficiary_id ON machine_tokens
FOR EACH ROW
EXECUTE FUNCTION mark_machine_token_beneficiary_ambiguous();
