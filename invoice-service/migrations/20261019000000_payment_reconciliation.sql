-- Payment reconciliation: attempts whose outcome is unknown are resolved by a
-- background worker that asks the PSP what happened.

ALTER TABLE payment_attempts
    -- Exact key sent to the PSP's /charge, used to look the charge up later.
    ADD COLUMN psp_idempotency_key VARCHAR(512),
    ADD COLUMN resolution_attempts SMALLINT NOT NULL DEFAULT 0,
    ADD COLUMN next_resolution_at TIMESTAMPTZ,
    ADD COLUMN resolved_at TIMESTAMPTZ;

-- Attempts recorded before this migration used "{invoice_id}-{idempotency_key}".
UPDATE payment_attempts
SET psp_idempotency_key = invoice_id::text || '-' || idempotency_key,
    next_resolution_at = CASE WHEN status = 'pending_resolution' THEN now() END
WHERE idempotency_key IS NOT NULL;

-- Not a UNIQUE index on invoice_id: databases from before this migration can
-- already hold several pending_resolution rows for one invoice. "One charge in
-- flight per invoice" is enforced under the invoice row lock in /pay instead.
-- Keeps the reconciler's scan limited to unresolved rows.
CREATE INDEX idx_payment_attempts_unresolved
    ON payment_attempts(next_resolution_at)
    WHERE status IN ('pending', 'pending_resolution');
