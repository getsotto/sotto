-- Preserve exact replay for provider receipts applied by migration 0026. Migration 0027 added
-- the association columns after the old apply path had already written completed attempts.
UPDATE cloud_provider_event_receipts AS receipt
SET collection_beneficiary_id = allocation.beneficiary_id,
    collection_attempt_id = 'provider-event:' || receipt.event_id,
    collection_run_id = 'legacy-provider-event-v1'
FROM cloud_provider_allocations AS allocation
WHERE receipt.status = 'applied'
  AND receipt.allocation_id = allocation.allocation_id
  AND receipt.collection_attempt_id IS NULL
  AND EXISTS (
      SELECT 1
      FROM cloud_coverage_collection_attempts AS attempt
      WHERE attempt.beneficiary_id = allocation.beneficiary_id
        AND attempt.attempt_id = 'provider-event:' || receipt.event_id
        AND attempt.status = 'completed'
  );
