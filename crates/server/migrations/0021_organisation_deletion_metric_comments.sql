-- Keep rationale for the fixed outcome vocabulary outside the original table migration so applied
-- migrations remain immutable while the operational explanation can still live with the schema.
COMMENT ON CONSTRAINT organization_deletion_metric_outcome_check
    ON organization_deletion_metric_counters IS
    'Cancellation does not record blocking outcomes because those observations retry as provider errors; retention and recovery do record blocking outcomes because they reconcile before another cancellation. Lease, compare-and-set, and purge metrics each use one bounded outcome set.';
