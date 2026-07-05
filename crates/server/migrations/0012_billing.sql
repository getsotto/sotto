-- Stripe billing linkage. Nullable: an org has these only once someone starts a subscription.
-- The tier column itself (0011) stays the single source of entitlement truth; billing.rs flips
-- it in response to verified Stripe webhooks.
ALTER TABLE organizations
    ADD COLUMN stripe_customer_id TEXT,
    ADD COLUMN stripe_subscription_id TEXT;

-- One org per Stripe customer/subscription; partial so the many NULLs don't collide.
CREATE UNIQUE INDEX organizations_stripe_customer_idx
    ON organizations (stripe_customer_id) WHERE stripe_customer_id IS NOT NULL;
CREATE UNIQUE INDEX organizations_stripe_subscription_idx
    ON organizations (stripe_subscription_id) WHERE stripe_subscription_id IS NOT NULL;
