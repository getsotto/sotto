-- Identifier-free preflight for the founding price-change notice control.
-- Run against a reviewed read-only production replica after migration 0065. This query returns
-- aggregate cohort counts only; do not add award, user, organisation, Stripe, or payment IDs.
WITH awards AS (
    SELECT award.award_id,
           award.original_end_epoch,
           job.state AS schedule_state,
           job.price_notice_eligible,
           notice.outcome AS notice_outcome,
           CASE
             WHEN award.original_end_epoch IS NULL THEN 'boundary_missing'
             WHEN job.award_id IS NULL THEN 'schedule_job_missing'
             WHEN job.state = 'scheduled' AND award.original_end_epoch - 5184000 > extract(epoch FROM now())::bigint
               THEN 'verified_transition_notice_window_future'
             WHEN job.state = 'scheduled' THEN 'verified_transition_notice_window_open_or_late'
             ELSE 'schedule_not_verified'
           END AS cohort
    FROM billing_founding_awards AS award
    LEFT JOIN billing_founding_schedule_jobs AS job ON job.award_id = award.award_id
    LEFT JOIN billing_founding_price_notice_state AS notice ON notice.award_id = award.award_id
)
SELECT cohort,
       count(*)::bigint AS award_count,
       count(*) FILTER (WHERE COALESCE(price_notice_eligible, FALSE) = FALSE)::bigint AS not_eligible_for_automatic_notice,
       count(*) FILTER (WHERE notice_outcome = 'queued')::bigint AS notice_queued,
       count(*) FILTER (WHERE notice_outcome = 'late_unresolved')::bigint AS notice_late_unresolved,
       count(*) FILTER (WHERE notice_outcome = 'schedule_unresolved')::bigint AS notice_schedule_unresolved
FROM awards
GROUP BY cohort
ORDER BY cohort;
