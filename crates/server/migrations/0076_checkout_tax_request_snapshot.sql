-- Checkout retries must preserve the tax treatment chosen when the provider request was saved.
-- Historical operations predate automatic tax and therefore retain the default false value.
ALTER TABLE billing_operations
    ADD COLUMN provider_checkout_automatic_tax_enabled BOOLEAN;
