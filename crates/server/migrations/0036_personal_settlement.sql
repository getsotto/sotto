ALTER TABLE billing_personal_accounts
    ADD COLUMN payment_reference TEXT UNIQUE,
    ADD COLUMN paid_through_date TEXT;

ALTER TABLE billing_personal_accounts
    ADD CONSTRAINT billing_personal_accounts_paid_date_pair CHECK (
        (paid_through_date IS NULL) = (paid_through_epoch IS NULL)
    );
