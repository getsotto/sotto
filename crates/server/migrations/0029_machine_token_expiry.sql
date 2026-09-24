-- Machine tokens expire.
--
-- Until now a machine token lived until someone revoked it, so a token nobody remembered (a
-- decommissioned pipeline, or one created by a member removed before removal revoked tokens)
-- stayed valid forever. Every token now carries an end date, after which it fails
-- authentication exactly like a revoked one and drops out of rotation coverage. The server sets
-- it at creation from a lifetime in days; the lifetime policy lives in `machine.rs`, not here.
--
-- Existing tokens get 90 days from this upgrade, not from their creation date: counting from
-- `created_at` would expire most of them the moment the migration ran, silently breaking CI that
-- was working a second earlier. Counting from the upgrade gives every operator the same notice
-- window, whenever they choose to upgrade. `now()` is the transaction start, so every existing
-- row gets the same instant.
--
-- The default only exists to backfill; dropping it afterwards means an insert that forgets the
-- expiry fails loudly instead of quietly inheriting a policy written into an immutable file.

ALTER TABLE machine_tokens
    ADD COLUMN IF NOT EXISTS expires_at TIMESTAMPTZ NOT NULL DEFAULT now() + interval '90 days';

ALTER TABLE machine_tokens ALTER COLUMN expires_at DROP DEFAULT;

ALTER TABLE machine_tokens DROP CONSTRAINT IF EXISTS machine_tokens_expiry_after_creation;
ALTER TABLE machine_tokens
    ADD CONSTRAINT machine_tokens_expiry_after_creation CHECK (expires_at > created_at);
