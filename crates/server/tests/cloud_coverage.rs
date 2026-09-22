use sotto_server::cloud_coverage::{
    evaluate, ConfirmedPaidInterval, CoverageDecision, CoverageState, InvalidCoverage,
    PersonCoverage, EXPORT_WINDOW_SECONDS, RENEWAL_RECOVERY_SECONDS,
};

const DAY: i64 = 24 * 60 * 60;

fn coverage(paid_intervals: Vec<ConfirmedPaidInterval>) -> PersonCoverage {
    PersonCoverage {
        beneficiary_id: "user-1".into(),
        paid_intervals,
    }
}

fn paid(id: &str, source: &str, start: i64, end: i64) -> ConfirmedPaidInterval {
    ConfirmedPaidInterval {
        coverage_id: id.into(),
        source_id: source.into(),
        starts_at: start,
        paid_until: end,
        failed_renewal_id: None,
    }
}

fn recovery(id: &str, source: &str, start: i64, end: i64, renewal: &str) -> ConfirmedPaidInterval {
    ConfirmedPaidInterval {
        failed_renewal_id: Some(renewal.into()),
        ..paid(id, source, start, end)
    }
}

#[test]
fn no_or_future_coverage_is_free() {
    assert_eq!(
        evaluate(&coverage(vec![]), 5 * DAY).unwrap().state,
        CoverageState::Free
    );
    assert_eq!(
        evaluate(
            &coverage(vec![paid("future", "personal", 10 * DAY, 40 * DAY)]),
            5 * DAY
        )
        .unwrap()
        .state,
        CoverageState::Free
    );
}

#[test]
fn paid_boundaries_and_cancellation_are_half_open() {
    let input = coverage(vec![paid("paid", "personal", 0, 30 * DAY)]);
    let during = evaluate(&input, 29 * DAY).unwrap();
    assert_eq!(during.state, CoverageState::Paid);
    assert_eq!(during.active_until, Some(30 * DAY));
    assert_eq!(
        evaluate(&input, 30 * DAY).unwrap().state,
        CoverageState::ExportOnly
    );
    let at_export_deadline = evaluate(&input, 60 * DAY).unwrap();
    assert_eq!(at_export_deadline.state, CoverageState::Expired);
    assert_eq!(at_export_deadline.export_until, Some(60 * DAY));
}

#[test]
fn failed_renewal_has_one_recovery_window_and_export_deadline() {
    let input = coverage(vec![recovery("paid", "personal", 0, 30 * DAY, "renewal-1")]);
    let recovery_end = 30 * DAY + RENEWAL_RECOVERY_SECONDS;
    let during = evaluate(&input, 30 * DAY).unwrap();
    assert_eq!(during.state, CoverageState::RenewalRecovery);
    assert_eq!(during.active_until, Some(recovery_end));
    assert_eq!(during.recovery_until, Some(recovery_end));

    let after = evaluate(&input, recovery_end).unwrap();
    assert_eq!(after.state, CoverageState::ExportOnly);
    assert_eq!(
        after.export_until,
        Some(recovery_end + EXPORT_WINDOW_SECONDS)
    );
    assert_eq!(
        evaluate(&input, 40 * DAY).unwrap().recovery_until,
        Some(recovery_end)
    );
}

#[test]
fn retried_failure_does_not_restart_recovery() {
    let fact = recovery("paid", "personal", 0, 30 * DAY, "renewal-1");
    let input = coverage(vec![fact.clone(), fact]);
    let at_recovery_end = evaluate(&input, 44 * DAY).unwrap();
    assert_eq!(at_recovery_end.state, CoverageState::ExportOnly);
    assert_eq!(at_recovery_end.export_until, Some(74 * DAY));
}

#[test]
fn paid_takes_precedence_and_sources_extend_one_episode() {
    let input = coverage(vec![
        recovery("personal", "personal", 0, 30 * DAY, "renewal-1"),
        paid("sponsor", "org-1", 20 * DAY, 50 * DAY),
    ]);
    let decision = evaluate(&input, 35 * DAY).unwrap();
    assert_eq!(decision.state, CoverageState::Paid);
    assert_eq!(decision.active_until, Some(50 * DAY));
    assert_eq!(decision.recovery_until, None);
    assert_eq!(
        evaluate(&input, 50 * DAY).unwrap().export_until,
        Some(50 * DAY + EXPORT_WINDOW_SECONDS)
    );
}

#[test]
fn future_paid_segment_excludes_recovery_from_current_decision() {
    let input = coverage(vec![
        recovery("renewal", "personal", 0, 30 * DAY, "renewal-1"),
        paid("next", "personal", 35 * DAY, 65 * DAY),
    ]);
    let decision = evaluate(&input, 36 * DAY).unwrap();
    assert_eq!(decision.state, CoverageState::Paid);
    assert_eq!(decision.active_until, Some(65 * DAY));
    assert_eq!(decision.recovery_until, None);
}

#[test]
fn touching_periods_are_continuous_but_future_periods_do_not_extend_old_expiry() {
    let touching = coverage(vec![
        paid("one", "personal", 0, 30 * DAY),
        paid("two", "personal", 30 * DAY, 60 * DAY),
    ]);
    assert_eq!(
        evaluate(&touching, 30 * DAY).unwrap().active_until,
        Some(60 * DAY)
    );
    assert_eq!(
        evaluate(&touching, 30 * DAY).unwrap().state,
        CoverageState::Paid
    );

    let gap = coverage(vec![
        paid("one", "personal", 0, 30 * DAY),
        paid("two", "personal", 100 * DAY, 130 * DAY),
    ]);
    let expired = evaluate(&gap, 70 * DAY).unwrap();
    assert_eq!(expired.state, CoverageState::Expired);
    assert_eq!(expired.export_until, Some(60 * DAY));
    assert_eq!(
        evaluate(&gap, 130 * DAY).unwrap().state,
        CoverageState::ExportOnly
    );
    assert_eq!(
        evaluate(&gap, 130 * DAY).unwrap().export_until,
        Some(160 * DAY)
    );
}

#[test]
fn malformed_input_fails_closed() {
    assert_eq!(
        evaluate(
            &PersonCoverage {
                beneficiary_id: String::new(),
                paid_intervals: vec![],
            },
            0
        )
        .unwrap_err(),
        InvalidCoverage::EmptyBeneficiary
    );
    assert_eq!(
        evaluate(&coverage(vec![paid("", "source", 0, 10)]), 1).unwrap_err(),
        InvalidCoverage::EmptyCoverageId
    );
    assert_eq!(
        evaluate(&coverage(vec![paid("coverage", "", 0, 10)]), 1).unwrap_err(),
        InvalidCoverage::EmptySourceId
    );
    assert_eq!(
        evaluate(
            &coverage(vec![recovery("coverage", "source", 0, 10, "")]),
            1
        )
        .unwrap_err(),
        InvalidCoverage::EmptyRenewalId
    );
    assert_eq!(
        evaluate(
            &coverage(vec![paid("same", "one", 0, 10), paid("same", "two", 0, 10)]),
            1
        )
        .unwrap_err(),
        InvalidCoverage::ConflictingCoverage
    );
    assert_eq!(
        evaluate(&coverage(vec![paid("bad", "source", 10, 10)]), 1).unwrap_err(),
        InvalidCoverage::InvalidInterval
    );
    assert_eq!(
        evaluate(
            &coverage(vec![recovery(
                "bad-renewal",
                "source",
                0,
                i64::MAX,
                "renewal"
            )]),
            1
        )
        .unwrap_err(),
        InvalidCoverage::RecoveryDeadlineOverflow
    );
    assert_eq!(
        evaluate(
            &coverage(vec![paid("bad-export", "source", i64::MAX - 1, i64::MAX,)]),
            i64::MAX
        )
        .unwrap_err(),
        InvalidCoverage::ExportDeadlineOverflow
    );
    assert_eq!(
        evaluate(
            &coverage(vec![
                recovery("first", "source", 0, 10, "renewal"),
                recovery("second", "source", 1, 10, "renewal"),
            ]),
            1
        )
        .unwrap_err(),
        InvalidCoverage::ConflictingRenewal
    );
}

#[test]
fn max_ending_interval_is_valid_before_and_during_but_overflows_export_at_end() {
    let input = coverage(vec![paid("max", "personal", i64::MAX - 1, i64::MAX)]);
    assert_eq!(
        evaluate(&input, i64::MAX - 2).unwrap(),
        CoverageDecision {
            state: CoverageState::Free,
            active_until: None,
            recovery_until: None,
            export_until: None,
        }
    );
    assert_eq!(
        evaluate(&input, i64::MAX - 1).unwrap(),
        CoverageDecision {
            state: CoverageState::Paid,
            active_until: Some(i64::MAX),
            recovery_until: None,
            export_until: None,
        }
    );
    assert_eq!(
        evaluate(&input, i64::MAX).unwrap_err(),
        InvalidCoverage::ExportDeadlineOverflow
    );
}

#[test]
fn export_deadline_fits_exactly_at_max_and_overflows_one_second_later() {
    let end = i64::MAX - EXPORT_WINDOW_SECONDS;
    let fitting = coverage(vec![paid("fitting", "personal", 0, end)]);
    let at_end = evaluate(&fitting, end).unwrap();
    assert_eq!(at_end.state, CoverageState::ExportOnly);
    assert_eq!(at_end.export_until, Some(i64::MAX));
    let expired = evaluate(&fitting, i64::MAX).unwrap();
    assert_eq!(expired.state, CoverageState::Expired);
    assert_eq!(expired.export_until, Some(i64::MAX));

    let overflowing = coverage(vec![paid("overflowing", "personal", 0, end + 1)]);
    assert_eq!(
        evaluate(&overflowing, end + 1).unwrap_err(),
        InvalidCoverage::ExportDeadlineOverflow
    );
}

#[test]
fn recovery_deadline_fits_exactly_at_max_and_overflows_one_second_later() {
    let paid_until = i64::MAX - RENEWAL_RECOVERY_SECONDS;
    let fitting = coverage(vec![recovery(
        "fitting",
        "personal",
        0,
        paid_until,
        "renewal-fitting",
    )]);
    let recovering = evaluate(&fitting, paid_until).unwrap();
    assert_eq!(recovering.state, CoverageState::RenewalRecovery);
    assert_eq!(recovering.active_until, Some(i64::MAX));
    assert_eq!(recovering.recovery_until, Some(i64::MAX));
    assert_eq!(
        evaluate(&fitting, i64::MAX).unwrap_err(),
        InvalidCoverage::ExportDeadlineOverflow
    );

    let overflowing = coverage(vec![recovery(
        "overflowing",
        "personal",
        0,
        paid_until + 1,
        "renewal-overflowing",
    )]);
    assert_eq!(
        evaluate(&overflowing, 1).unwrap_err(),
        InvalidCoverage::RecoveryDeadlineOverflow
    );
}

#[test]
fn duplicate_identical_facts_are_idempotent_and_input_order_does_not_matter() {
    let one = paid("same", "source", 0, 30 * DAY);
    let left = coverage(vec![
        one.clone(),
        one.clone(),
        paid("other", "source", 35 * DAY, 65 * DAY),
    ]);
    let right = coverage(vec![paid("other", "source", 35 * DAY, 65 * DAY), one]);
    assert_eq!(evaluate(&left, 20 * DAY), evaluate(&right, 20 * DAY));
}
