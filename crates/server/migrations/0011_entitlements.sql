-- Org entitlements (M6 PR5): tiers, the 14-day Team trial, and server-enforced quotas.
--
-- The org is the paid unit. `tier` is what's been (manually, for now — Stripe is a later, thin
-- PR) assigned; the *effective* tier is `team` while `tier = 'team'` OR the trial is still
-- running. Every new org starts a 14-day Team trial (set at creation), then drops to the free
-- limits unless upgraded. Existing orgs backfill to `free` with no trial — pre-launch, that's
-- only test data.
--
-- Free limits live in code (`entitlements.rs`), deliberately tight from day one: loosening later
-- delights, tightening enrages. Personal projects and share links are never limited — they are
-- the viral funnel.

ALTER TABLE organizations
    ADD COLUMN IF NOT EXISTS tier TEXT NOT NULL DEFAULT 'free' CHECK (tier IN ('free', 'team'));

ALTER TABLE organizations
    ADD COLUMN IF NOT EXISTS trial_ends_at TIMESTAMPTZ;
