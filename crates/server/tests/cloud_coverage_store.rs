use std::{str::FromStr, sync::Arc};

use sotto_server::cloud_coverage::{evaluate, ConfirmedPaidInterval, CoverageState};
use sotto_server::cloud_coverage_store::{
    load, publish, CoverageProjection, PublicationOutcome, StoreError, UnavailableReason,
};
use sotto_server::db;
use sqlx::postgres::PgConnectOptions;
use sqlx::PgPool;
use tokio::sync::{oneshot, Barrier, Notify};
use tokio::time::Duration;
use uuid::Uuid;

mod support;

use support::coverage_concurrency::{
    receive_owned, receive_pid, run_with_context, run_with_teardown,
    run_with_teardown_with_budgets, transaction_pid, wait_for_specific_block, RaceTaskOwner,
    RACE_TIMEOUT,
};

const DAY: i64 = 24 * 60 * 60;

struct Fixture {
    pool: PgPool,
    beneficiary_id: String,
}

impl Fixture {
    async fn create() -> Option<Self> {
        if std::env::var("SOTTO_RUN_DB_TESTS").as_deref() != Ok("1") {
            eprintln!("skipping cloud coverage store test: set SOTTO_RUN_DB_TESTS=1");
            return None;
        }
        let database_url = std::env::var("DATABASE_URL")
            .expect("DATABASE_URL is required when SOTTO_RUN_DB_TESTS=1");
        let options = PgConnectOptions::from_str(&database_url).expect("parse DATABASE_URL");
        assert!(
            matches!(options.get_host(), "localhost" | "127.0.0.1" | "::1"),
            "refusing coverage store tests against non-local host: {}",
            options.get_host()
        );
        let pool = db::connect(&database_url).await.expect("connect");
        db::migrate(&pool).await.expect("migrate");
        let beneficiary_id = format!("coverage-store-test-{}", Uuid::new_v4());
        sqlx::query(
            "INSERT INTO users (id, oauth_provider, oauth_subject) VALUES ($1, 'coverage-test', $2)",
        )
        .bind(&beneficiary_id)
        .bind(&beneficiary_id)
        .execute(&pool)
        .await
        .expect("insert coverage test user");
        Some(Self {
            pool,
            beneficiary_id,
        })
    }

    async fn create_owned(owner: &mut RaceTaskOwner) -> Result<Option<Self>, String> {
        if std::env::var("SOTTO_RUN_DB_TESTS").as_deref() != Ok("1") {
            eprintln!("skipping cloud coverage store test: set SOTTO_RUN_DB_TESTS=1");
            return Ok(None);
        }
        let database_url = std::env::var("DATABASE_URL")
            .map_err(|_| "DATABASE_URL is required when SOTTO_RUN_DB_TESTS=1".to_string())?;
        let options = PgConnectOptions::from_str(&database_url)
            .map_err(|error| format!("parse DATABASE_URL: {error}"))?;
        if !matches!(options.get_host(), "localhost" | "127.0.0.1" | "::1") {
            return Err(format!(
                "refusing coverage store tests against non-local host: {}",
                options.get_host()
            ));
        }
        let pool = db::connect(&database_url)
            .await
            .map_err(|error| format!("connect: {error}"))?;
        db::migrate(&pool)
            .await
            .map_err(|error| format!("migrate: {error}"))?;
        let beneficiary_id = format!("coverage-store-test-{}", Uuid::new_v4());
        let cleanup_pool = pool.clone();
        let cleanup_beneficiary = beneficiary_id.clone();
        owner.register_cleanup(move || async move {
            cleanup_result_for(&cleanup_pool, &cleanup_beneficiary).await
        });
        let insert_result = sqlx::query(
            "INSERT INTO users (id, oauth_provider, oauth_subject) VALUES ($1, 'coverage-test', $2)",
        )
        .bind(&beneficiary_id)
        .bind(&beneficiary_id)
        .execute(&pool)
        .await;
        if let Err(error) = insert_result {
            let error = format!("insert coverage test user: {error}");
            return match owner.cleanup_registered().await {
                Ok(()) => Err(error),
                Err(cleanup) => Err(format!("{error}; cleanup: {cleanup}")),
            };
        }
        Ok(Some(Self {
            pool,
            beneficiary_id,
        }))
    }
}

async fn cleanup(fixture: &Fixture) {
    cleanup_result(fixture)
        .await
        .expect("delete coverage test fixture");
}

async fn cleanup_result(fixture: &Fixture) -> Result<(), String> {
    cleanup_result_for(&fixture.pool, &fixture.beneficiary_id).await
}

async fn cleanup_result_for(pool: &PgPool, beneficiary_id: &str) -> Result<(), String> {
    sqlx::query("DELETE FROM cloud_coverage_heads WHERE beneficiary_id = $1")
        .bind(beneficiary_id)
        .execute(pool)
        .await
        .map_err(|error| format!("delete coverage head: {error}"))?;
    sqlx::query("DELETE FROM cloud_coverage_revision_facts WHERE beneficiary_id = $1")
        .bind(beneficiary_id)
        .execute(pool)
        .await
        .map_err(|error| format!("delete coverage facts: {error}"))?;
    sqlx::query("DELETE FROM cloud_coverage_revisions WHERE beneficiary_id = $1")
        .bind(beneficiary_id)
        .execute(pool)
        .await
        .map_err(|error| format!("delete coverage revisions: {error}"))?;
    sqlx::query("DELETE FROM users WHERE id = $1")
        .bind(beneficiary_id)
        .execute(pool)
        .await
        .map_err(|error| format!("delete coverage test user: {error}"))?;
    Ok(())
}

fn paid(id: &str, source: &str, starts_at: i64, paid_until: i64) -> ConfirmedPaidInterval {
    ConfirmedPaidInterval {
        coverage_id: id.into(),
        source_id: source.into(),
        starts_at,
        paid_until,
        failed_renewal_id: None,
    }
}

fn recovery(
    id: &str,
    source: &str,
    starts_at: i64,
    paid_until: i64,
    renewal: &str,
) -> ConfirmedPaidInterval {
    ConfirmedPaidInterval {
        failed_renewal_id: Some(renewal.into()),
        ..paid(id, source, starts_at, paid_until)
    }
}

async fn committed_publish(
    fixture: &Fixture,
    expected_revision: Option<i64>,
    operation_id: &str,
    evidence_reference: &str,
    projection: &CoverageProjection,
) -> Result<sotto_server::cloud_coverage_store::PublicationReceipt, StoreError> {
    let mut tx = fixture.pool.begin().await.expect("begin publication");
    let receipt = publish(
        &mut tx,
        &fixture.beneficiary_id,
        expected_revision,
        operation_id,
        evidence_reference,
        projection,
    )
    .await?;
    tx.commit().await.expect("commit publication");
    Ok(receipt)
}

#[tokio::test]
async fn missing_and_complete_empty_projection_are_distinct() {
    let Some(fixture) = Fixture::create().await else {
        return;
    };
    assert!(matches!(
        load(&fixture.pool, &fixture.beneficiary_id).await,
        Err(StoreError::ProjectionMissing)
    ));

    let receipt = committed_publish(
        &fixture,
        None,
        "empty-op",
        "empty-evidence",
        &CoverageProjection::Complete {
            paid_intervals: vec![],
        },
    )
    .await
    .expect("publish empty projection");
    assert_eq!(receipt.revision, 1);
    let loaded = load(&fixture.pool, &fixture.beneficiary_id)
        .await
        .expect("load empty projection");
    assert_eq!(loaded.coverage.paid_intervals, Vec::new());
    assert_eq!(
        evaluate(&loaded.coverage, 5 * DAY).unwrap().state,
        CoverageState::Free
    );
    cleanup(&fixture).await;
}

#[tokio::test]
async fn complete_facts_round_trip_and_exact_replay_is_idempotent() {
    let Some(fixture) = Fixture::create().await else {
        return;
    };
    let facts = vec![
        paid("B", "sponsor", 0, 30 * DAY),
        paid("a", "sponsor", 40 * DAY, 70 * DAY),
    ];
    let first = committed_publish(
        &fixture,
        None,
        "operation-1",
        "evidence-1",
        &CoverageProjection::Complete {
            paid_intervals: facts.clone(),
        },
    )
    .await
    .expect("publish complete projection");
    assert_eq!(first.outcome, PublicationOutcome::Applied);

    let mut reordered = facts;
    reordered.reverse();
    reordered.push(reordered[0].clone());
    let replay = committed_publish(
        &fixture,
        None,
        "operation-1",
        "evidence-1",
        &CoverageProjection::Complete {
            paid_intervals: reordered,
        },
    )
    .await
    .expect("replay complete projection");
    assert_eq!(replay.revision, first.revision);
    assert_eq!(replay.outcome, PublicationOutcome::AlreadyApplied);

    let loaded = load(&fixture.pool, &fixture.beneficiary_id)
        .await
        .expect("load complete projection");
    assert_eq!(loaded.revision, 1);
    assert_eq!(loaded.coverage.paid_intervals.len(), 2);
    assert_eq!(loaded.coverage.paid_intervals[0].coverage_id, "B");
    assert_eq!(loaded.coverage.paid_intervals[1].coverage_id, "a");
    cleanup(&fixture).await;
}

#[tokio::test]
async fn operation_conflict_and_stale_replay_cannot_rewind_head() {
    let Some(fixture) = Fixture::create().await else {
        return;
    };
    let first_projection = CoverageProjection::Complete {
        paid_intervals: vec![paid("first", "personal", 0, 30 * DAY)],
    };
    committed_publish(
        &fixture,
        None,
        "operation-1",
        "evidence-1",
        &first_projection,
    )
    .await
    .expect("publish first projection");
    committed_publish(
        &fixture,
        Some(1),
        "operation-2",
        "evidence-2",
        &CoverageProjection::Complete {
            paid_intervals: vec![paid("second", "personal", 30 * DAY, 60 * DAY)],
        },
    )
    .await
    .expect("publish correction");
    assert!(matches!(
        committed_publish(
            &fixture,
            Some(1),
            "competing-correction",
            "evidence-competing",
            &CoverageProjection::Complete {
                paid_intervals: vec![paid("competing", "personal", 30 * DAY, 90 * DAY)],
            },
        )
        .await,
        Err(StoreError::RevisionConflict {
            expected: Some(1),
            actual: Some(2),
        })
    ));

    let replay = committed_publish(
        &fixture,
        None,
        "operation-1",
        "evidence-1",
        &first_projection,
    )
    .await
    .expect("replay old operation");
    assert_eq!(replay.outcome, PublicationOutcome::AlreadyApplied);
    assert_eq!(replay.revision, 1);
    assert!(matches!(
        committed_publish(
            &fixture,
            None,
            "operation-1",
            "changed-evidence",
            &first_projection,
        )
        .await,
        Err(StoreError::OperationConflict)
    ));
    assert!(matches!(
        committed_publish(
            &fixture,
            None,
            "operation-1",
            "evidence-1",
            &CoverageProjection::Complete {
                paid_intervals: vec![paid("different-facts", "personal", 0, 30 * DAY)],
            },
        )
        .await,
        Err(StoreError::OperationConflict)
    ));
    assert!(matches!(
        committed_publish(
            &fixture,
            None,
            "stale-operation",
            "stale-evidence",
            &first_projection,
        )
        .await,
        Err(StoreError::RevisionConflict {
            expected: None,
            actual: Some(2),
        })
    ));
    assert_eq!(
        load(&fixture.pool, &fixture.beneficiary_id)
            .await
            .unwrap()
            .revision,
        2
    );
    cleanup(&fixture).await;
}

#[tokio::test]
async fn competing_corrections_serialize_on_the_head_and_reject_the_loser() {
    let Some(fixture) = Fixture::create().await else {
        return;
    };
    committed_publish(
        &fixture,
        None,
        "correction-base",
        "correction-base-evidence",
        &CoverageProjection::Complete {
            paid_intervals: vec![paid("correction-base-fact", "personal", 0, 30 * DAY)],
        },
    )
    .await
    .expect("publish correction base");

    let winning_projection = CoverageProjection::Complete {
        paid_intervals: vec![paid(
            "correction-winner-fact",
            "personal",
            30 * DAY,
            60 * DAY,
        )],
    };
    let losing_projection = CoverageProjection::Complete {
        paid_intervals: vec![paid(
            "correction-loser-fact",
            "personal",
            60 * DAY,
            90 * DAY,
        )],
    };
    let release = Arc::new(Notify::new());
    let mut owner = RaceTaskOwner::new();
    let cleanup_pool = fixture.pool.clone();
    let cleanup_beneficiary = fixture.beneficiary_id.clone();
    owner.register_cleanup(move || async move {
        cleanup_result_for(&cleanup_pool, &cleanup_beneficiary).await
    });
    let result = run_with_context(
        &mut owner,
        |owner| {
            Box::pin(async move {
                let (holder_ready, holder_ready_rx) = oneshot::channel();
                let holder_pool = fixture.pool.clone();
                let holder_beneficiary = fixture.beneficiary_id.clone();
                let holder_projection = winning_projection.clone();
                let holder_release = release.clone();
                let mut holder = Some(owner.spawn(async move {
                    let mut tx = holder_pool.begin().await.expect("begin held correction");
                    let pid = transaction_pid(&mut tx).await;
                    let result = publish(
                        &mut tx,
                        &holder_beneficiary,
                        Some(1),
                        "correction-winner",
                        "correction-winner-evidence",
                        &holder_projection,
                    )
                    .await;
                    holder_ready.send(pid).expect("signal held correction");
                    holder_release.notified().await;
                    match result {
                        Ok(receipt) => {
                            tx.commit().await.expect("commit held correction");
                            Ok(receipt)
                        }
                        Err(error) => {
                            tx.rollback().await.expect("rollback held correction");
                            Err(error)
                        }
                    }
                }));
                let holder_pid =
                    receive_pid(holder_ready_rx, "receive correction holder pid").await;

                let (waiter_ready, waiter_ready_rx) = oneshot::channel();
                let waiter_pool = fixture.pool.clone();
                let waiter_beneficiary = fixture.beneficiary_id.clone();
                let mut waiter = Some(owner.spawn(async move {
                    let mut tx = waiter_pool.begin().await.expect("begin waiting correction");
                    let pid = transaction_pid(&mut tx).await;
                    waiter_ready.send(pid).expect("signal waiting correction");
                    let result = publish(
                        &mut tx,
                        &waiter_beneficiary,
                        Some(1),
                        "correction-loser",
                        "correction-loser-evidence",
                        &losing_projection,
                    )
                    .await;
                    tx.rollback().await.expect("rollback waiting correction");
                    result
                }));
                let waiter_pid =
                    receive_pid(waiter_ready_rx, "receive correction waiter pid").await;
                wait_for_specific_block(&fixture.pool, waiter_pid, holder_pid).await;
                release.notify_one();

                let winner = receive_owned(&mut holder, "held correction")
                    .await
                    .expect("held correction task completed")
                    .expect("winning correction applied");
                let loser = receive_owned(&mut waiter, "waiting correction")
                    .await
                    .expect("waiting correction task completed");
                assert_eq!(winner.outcome, PublicationOutcome::Applied);
                assert_eq!(winner.revision, 2);
                assert!(matches!(
                    loser,
                    Err(StoreError::RevisionConflict {
                        expected: Some(1),
                        actual: Some(2),
                    })
                ));
                let loaded = load(&fixture.pool, &fixture.beneficiary_id)
                    .await
                    .expect("load winning correction");
                assert_eq!(loaded.revision, 2);
                assert_eq!(loaded.coverage.paid_intervals.len(), 1);
                assert_eq!(
                    loaded.coverage.paid_intervals[0].coverage_id,
                    "correction-winner-fact"
                );
                let historical: (String, i64) = sqlx::query_as(
                    "SELECT operation_id, fact_count FROM cloud_coverage_revisions \
                     WHERE beneficiary_id = $1 AND revision = 1",
                )
                .bind(&fixture.beneficiary_id)
                .fetch_one(&fixture.pool)
                .await
                .expect("read preserved correction history");
                assert_eq!(historical, ("correction-base".into(), 1));
                let historical_fact: String = sqlx::query_scalar(
                    "SELECT coverage_id FROM cloud_coverage_revision_facts \
                     WHERE beneficiary_id = $1 AND revision = 1",
                )
                .bind(&fixture.beneficiary_id)
                .fetch_one(&fixture.pool)
                .await
                .expect("read preserved correction fact");
                assert_eq!(historical_fact, "correction-base-fact");
                let loser_count: i64 = sqlx::query_scalar(
                    "SELECT count(*) FROM cloud_coverage_revisions \
                     WHERE beneficiary_id = $1 AND operation_id = 'correction-loser'",
                )
                .bind(&fixture.beneficiary_id)
                .fetch_one(&fixture.pool)
                .await
                .expect("count losing correction");
                assert_eq!(loser_count, 0);
                Ok(())
            })
        },
        || async { Ok::<(), String>(()) },
    )
    .await;
    result.expect("supervised correction race");
}

#[tokio::test]
async fn aborted_owned_publication_task_rolls_back_before_fixture_cleanup() {
    let Some(fixture) = Fixture::create().await else {
        return;
    };
    let Some(unrelated) = Fixture::create().await else {
        return;
    };
    committed_publish(
        &unrelated,
        None,
        "unrelated-publication",
        "unrelated-evidence",
        &CoverageProjection::Complete {
            paid_intervals: vec![],
        },
    )
    .await
    .expect("publish unrelated fixture");

    let mut owner = RaceTaskOwner::new();
    let cleanup_pool = fixture.pool.clone();
    let cleanup_beneficiary = fixture.beneficiary_id.clone();
    let unrelated_pool = unrelated.pool.clone();
    let unrelated_beneficiary = unrelated.beneficiary_id.clone();
    owner.register_cleanup(move || async move {
        cleanup_result_for(&cleanup_pool, &cleanup_beneficiary).await?;
        cleanup_result_for(&unrelated_pool, &unrelated_beneficiary).await
    });
    let result = run_with_context(
        &mut owner,
        |owner| {
            Box::pin(async move {
                let (ready, ready_rx) = oneshot::channel();
                let (release, release_rx) = oneshot::channel();
                let (finished, finished_rx) = oneshot::channel();
                let pool = fixture.pool.clone();
                let beneficiary_id = fixture.beneficiary_id.clone();
                let _task = owner.spawn(async move {
                    let mut tx = pool.begin().await.expect("begin owned publication");
                    publish(
                        &mut tx,
                        &beneficiary_id,
                        None,
                        "aborted-publication",
                        "aborted-evidence",
                        &CoverageProjection::Complete {
                            paid_intervals: vec![],
                        },
                    )
                    .await
                    .expect("publish owned publication");
                    ready.send(()).expect("signal owned publication");
                    release_rx.await.expect("release owned publication");
                    finished
                        .send(())
                        .expect("signal owned publication completion");
                });
                tokio::time::timeout(RACE_TIMEOUT, ready_rx)
                    .await
                    .expect("owned publication became ready")
                    .expect("owned publication task exited before readiness");
                release.send(()).expect("release owned publication");
                finished_rx.await.expect("owned publication task completed");
                let receipt = committed_publish(
                    &fixture,
                    None,
                    "aborted-publication",
                    "aborted-evidence",
                    &CoverageProjection::Complete {
                        paid_intervals: vec![],
                    },
                )
                .await
                .expect("replacement publication after task cleanup");
                assert_eq!(receipt.revision, 1);
                assert_eq!(
                    load(&fixture.pool, &fixture.beneficiary_id)
                        .await
                        .expect("load replacement publication")
                        .revision,
                    receipt.revision
                );
                let unrelated_loaded = load(&unrelated.pool, &unrelated.beneficiary_id)
                    .await
                    .expect("load unrelated fixture after cleanup");
                assert_eq!(unrelated_loaded.revision, 1);
                Ok(())
            })
        },
        || async { Ok::<(), String>(()) },
    )
    .await;
    result.expect("supervised publication cleanup");
}

#[tokio::test]
async fn scenario_panic_cleans_owned_publication_fixture() {
    let Some(unrelated) = Fixture::create().await else {
        return;
    };
    committed_publish(
        &unrelated,
        None,
        "unrelated-publication",
        "unrelated-evidence",
        &CoverageProjection::Complete {
            paid_intervals: vec![],
        },
    )
    .await
    .expect("publish unrelated fixture");
    let mut owner = RaceTaskOwner::new();
    let Some(fixture) = Fixture::create_owned(&mut owner)
        .await
        .expect("create owned fixture")
    else {
        cleanup(&unrelated).await;
        return;
    };

    let (ready, ready_rx) = oneshot::channel();
    let pool = fixture.pool.clone();
    let beneficiary_id = fixture.beneficiary_id.clone();
    let _task = owner.spawn(async move {
        let mut tx = pool.begin().await.expect("begin panic fixture publication");
        publish(
            &mut tx,
            &beneficiary_id,
            None,
            "panic-publication",
            "panic-evidence",
            &CoverageProjection::Complete {
                paid_intervals: vec![],
            },
        )
        .await
        .expect("publish panic fixture");
        ready.send(()).expect("signal panic fixture readiness");
        std::future::pending::<()>().await;
    });

    let result = run_with_teardown(
        &mut owner,
        async {
            tokio::time::timeout(RACE_TIMEOUT, ready_rx)
                .await
                .map_err(|_| "timed out waiting for panic fixture".to_string())
                .and_then(|result| {
                    result.map_err(|_| "panic fixture task exited before readiness".to_string())
                })?;
            panic!("intentional scenario failure");
            #[allow(unreachable_code)]
            Ok::<(), String>(())
        },
        || async { Ok::<(), String>(()) },
    )
    .await;
    assert_eq!(
        result,
        Err("scenario: scenario panicked: intentional scenario failure".into())
    );

    let remaining_user: Option<String> = sqlx::query_scalar("SELECT id FROM users WHERE id = $1")
        .bind(&fixture.beneficiary_id)
        .fetch_optional(&fixture.pool)
        .await
        .expect("check panic fixture cleanup");
    assert!(remaining_user.is_none());
    let unrelated_loaded = load(&unrelated.pool, &unrelated.beneficiary_id)
        .await
        .expect("load unrelated fixture after panic cleanup");
    assert_eq!(unrelated_loaded.revision, 1);

    cleanup(&unrelated).await;
}

#[tokio::test]
async fn setup_failure_after_insert_cleans_the_registered_fixture() {
    let mut owner = RaceTaskOwner::new();
    let Some(fixture) = Fixture::create_owned(&mut owner)
        .await
        .expect("create owned fixture")
    else {
        return;
    };
    let result = run_with_teardown(
        &mut owner,
        async { Err::<(), _>("setup failed after user insert".into()) },
        || async { Ok::<(), String>(()) },
    )
    .await;
    assert_eq!(
        result,
        Err("scenario: setup failed after user insert".into())
    );
    let remaining_user: Option<String> = sqlx::query_scalar("SELECT id FROM users WHERE id = $1")
        .bind(&fixture.beneficiary_id)
        .fetch_optional(&fixture.pool)
        .await
        .expect("check setup failure cleanup");
    assert!(remaining_user.is_none());
}

#[tokio::test]
async fn readiness_timeout_cancels_a_parked_publication_before_cleanup() {
    let mut owner = RaceTaskOwner::new();
    let Some(fixture) = Fixture::create_owned(&mut owner)
        .await
        .expect("create owned fixture")
    else {
        return;
    };
    let (ready, ready_rx) = oneshot::channel();
    let pool = fixture.pool.clone();
    let beneficiary_id = fixture.beneficiary_id.clone();
    let _task = owner.spawn(async move {
        let mut tx = pool.begin().await.expect("begin parked publication");
        publish(
            &mut tx,
            &beneficiary_id,
            None,
            "readiness-timeout-publication",
            "readiness-timeout-evidence",
            &CoverageProjection::Complete {
                paid_intervals: vec![],
            },
        )
        .await
        .expect("publish parked publication");
        ready.send(()).expect("signal parked publication");
        std::future::pending::<()>().await;
    });

    let result = run_with_teardown_with_budgets(
        &mut owner,
        async {
            tokio::time::timeout(RACE_TIMEOUT, ready_rx)
                .await
                .map_err(|_| "parked publication did not become ready".to_string())
                .and_then(|result| {
                    result.map_err(|_| "parked publication exited early".to_string())
                })?;
            tokio::time::timeout(Duration::from_millis(10), std::future::pending::<()>())
                .await
                .map_err(|_| "readiness timed out".to_string())?;
            Ok(())
        },
        || async { Ok::<(), String>(()) },
        RACE_TIMEOUT,
        RACE_TIMEOUT,
    )
    .await;
    assert_eq!(result, Err("scenario: readiness timed out".into()));
    let remaining_user: Option<String> = sqlx::query_scalar("SELECT id FROM users WHERE id = $1")
        .bind(&fixture.beneficiary_id)
        .fetch_optional(&fixture.pool)
        .await
        .expect("check readiness timeout cleanup");
    assert!(remaining_user.is_none());
}

#[tokio::test]
async fn invalid_publication_does_not_create_a_head() {
    let Some(fixture) = Fixture::create().await else {
        return;
    };
    let mut tx = fixture
        .pool
        .begin()
        .await
        .expect("begin invalid publication");
    assert!(matches!(
        publish(
            &mut tx,
            &fixture.beneficiary_id,
            None,
            "invalid",
            "evidence-invalid",
            &CoverageProjection::Complete {
                paid_intervals: vec![paid("invalid", "personal", 10, 10)],
            },
        )
        .await,
        Err(StoreError::InvalidCoverage(
            sotto_server::cloud_coverage::InvalidCoverage::InvalidInterval
        ))
    ));
    tx.commit()
        .await
        .expect("commit invalid publication transaction");
    assert!(matches!(
        load(&fixture.pool, &fixture.beneficiary_id).await,
        Err(StoreError::ProjectionMissing)
    ));
    cleanup(&fixture).await;
}

#[tokio::test]
async fn first_revision_conflict_does_not_leave_an_empty_head() {
    let Some(fixture) = Fixture::create().await else {
        return;
    };
    let mut tx = fixture
        .pool
        .begin()
        .await
        .expect("begin first revision conflict");
    assert!(matches!(
        publish(
            &mut tx,
            &fixture.beneficiary_id,
            Some(1),
            "wrong-first-revision",
            "evidence-wrong-first-revision",
            &CoverageProjection::Complete {
                paid_intervals: vec![],
            },
        )
        .await,
        Err(StoreError::RevisionConflict {
            expected: Some(1),
            actual: None,
        })
    ));
    tx.commit()
        .await
        .expect("commit unrelated work after conflict");
    assert!(matches!(
        load(&fixture.pool, &fixture.beneficiary_id).await,
        Err(StoreError::ProjectionMissing)
    ));
    cleanup(&fixture).await;
}

#[tokio::test]
async fn simultaneous_first_publications_have_one_winner() {
    let Some(fixture) = Fixture::create().await else {
        return;
    };
    let barrier = Arc::new(Barrier::new(2));
    let first_pool = fixture.pool.clone();
    let second_pool = fixture.pool.clone();
    let beneficiary = fixture.beneficiary_id.clone();
    let first_barrier = barrier.clone();
    let second_barrier = barrier;
    let publish_one = async move {
        let mut tx = first_pool.begin().await.expect("begin first race");
        first_barrier.wait().await;
        let result = publish(
            &mut tx,
            &beneficiary,
            None,
            "race-one",
            "race-evidence-one",
            &CoverageProjection::Complete {
                paid_intervals: vec![paid("one", "personal", 0, 30 * DAY)],
            },
        )
        .await;
        match result {
            Ok(receipt) => {
                tx.commit().await.expect("commit first race");
                Ok(receipt)
            }
            Err(error) => {
                tx.rollback().await.expect("rollback first race");
                Err(error)
            }
        }
    };
    let beneficiary = fixture.beneficiary_id.clone();
    let publish_two = async move {
        let mut tx = second_pool.begin().await.expect("begin second race");
        second_barrier.wait().await;
        let result = publish(
            &mut tx,
            &beneficiary,
            None,
            "race-two",
            "race-evidence-two",
            &CoverageProjection::Complete {
                paid_intervals: vec![paid("two", "personal", 0, 30 * DAY)],
            },
        )
        .await;
        match result {
            Ok(receipt) => {
                tx.commit().await.expect("commit second race");
                Ok(receipt)
            }
            Err(error) => {
                tx.rollback().await.expect("rollback second race");
                Err(error)
            }
        }
    };
    let (first, second) = tokio::join!(publish_one, publish_two);
    let results = [first, second];
    assert_eq!(
        results
            .iter()
            .filter(|result| matches!(result, Ok(receipt) if receipt.outcome == PublicationOutcome::Applied))
            .count(),
        1
    );
    assert_eq!(
        results
            .iter()
            .filter(|result| matches!(result, Err(StoreError::RevisionConflict { .. })))
            .count(),
        1
    );
    cleanup(&fixture).await;
}

#[tokio::test]
async fn simultaneous_first_conflicts_retry_after_empty_head_cleanup() {
    let Some(fixture) = Fixture::create().await else {
        return;
    };
    let barrier = Arc::new(Barrier::new(2));
    let first_pool = fixture.pool.clone();
    let second_pool = fixture.pool.clone();
    let first_beneficiary = fixture.beneficiary_id.clone();
    let second_beneficiary = fixture.beneficiary_id.clone();
    let first_barrier = barrier.clone();
    let second_barrier = barrier;
    let publish_one = async move {
        let mut tx = first_pool.begin().await.expect("begin first conflict race");
        first_barrier.wait().await;
        let result = publish(
            &mut tx,
            &first_beneficiary,
            Some(1),
            "first-conflict-one",
            "first-conflict-evidence-one",
            &CoverageProjection::Complete {
                paid_intervals: vec![],
            },
        )
        .await;
        tx.commit().await.expect("commit first conflict cleanup");
        result
    };
    let publish_two = async move {
        let mut tx = second_pool
            .begin()
            .await
            .expect("begin second conflict race");
        second_barrier.wait().await;
        let result = publish(
            &mut tx,
            &second_beneficiary,
            Some(1),
            "first-conflict-two",
            "first-conflict-evidence-two",
            &CoverageProjection::Complete {
                paid_intervals: vec![],
            },
        )
        .await;
        tx.commit().await.expect("commit second conflict cleanup");
        result
    };
    let (first, second) = tokio::join!(publish_one, publish_two);
    assert!(matches!(
        first,
        Err(StoreError::RevisionConflict {
            expected: Some(1),
            actual: None,
        })
    ));
    assert!(matches!(
        second,
        Err(StoreError::RevisionConflict {
            expected: Some(1),
            actual: None,
        })
    ));
    assert!(matches!(
        load(&fixture.pool, &fixture.beneficiary_id).await,
        Err(StoreError::ProjectionMissing)
    ));
    cleanup(&fixture).await;
}

#[tokio::test]
async fn simultaneous_replays_of_one_operation_have_one_application() {
    let Some(fixture) = Fixture::create().await else {
        return;
    };
    let barrier = Arc::new(Barrier::new(2));
    let first_pool = fixture.pool.clone();
    let second_pool = fixture.pool.clone();
    let first_beneficiary = fixture.beneficiary_id.clone();
    let second_beneficiary = fixture.beneficiary_id.clone();
    let first_barrier = barrier.clone();
    let second_barrier = barrier;
    let publish_one = async move {
        let mut tx = first_pool.begin().await.expect("begin first replay race");
        first_barrier.wait().await;
        let result = publish(
            &mut tx,
            &first_beneficiary,
            None,
            "same-operation",
            "same-evidence",
            &CoverageProjection::Complete {
                paid_intervals: vec![paid("same", "personal", 0, 30 * DAY)],
            },
        )
        .await;
        match result {
            Ok(receipt) => {
                tx.commit().await.expect("commit first replay race");
                Ok(receipt)
            }
            Err(error) => {
                tx.rollback().await.expect("rollback first replay race");
                Err(error)
            }
        }
    };
    let publish_two = async move {
        let mut tx = second_pool.begin().await.expect("begin second replay race");
        second_barrier.wait().await;
        let result = publish(
            &mut tx,
            &second_beneficiary,
            None,
            "same-operation",
            "same-evidence",
            &CoverageProjection::Complete {
                paid_intervals: vec![paid("same", "personal", 0, 30 * DAY)],
            },
        )
        .await;
        match result {
            Ok(receipt) => {
                tx.commit().await.expect("commit second replay race");
                Ok(receipt)
            }
            Err(error) => {
                tx.rollback().await.expect("rollback second replay race");
                Err(error)
            }
        }
    };
    let (first, second) = tokio::join!(publish_one, publish_two);
    let results = [first, second];
    assert_eq!(
        results
            .iter()
            .filter(|result| matches!(result, Ok(receipt) if receipt.outcome == PublicationOutcome::Applied))
            .count(),
        1
    );
    assert_eq!(
        results
            .iter()
            .filter(|result| matches!(result, Ok(receipt) if receipt.outcome == PublicationOutcome::AlreadyApplied))
            .count(),
        1
    );
    cleanup(&fixture).await;
}

#[tokio::test]
async fn unavailable_projection_blocks_load_until_complete_revision() {
    let Some(fixture) = Fixture::create().await else {
        return;
    };
    committed_publish(
        &fixture,
        None,
        "unavailable-op",
        "reconcile-1",
        &CoverageProjection::Unavailable {
            reason: UnavailableReason::NeedsReconciliation,
        },
    )
    .await
    .expect("publish unavailable projection");
    assert!(matches!(
        load(&fixture.pool, &fixture.beneficiary_id).await,
        Err(StoreError::ProjectionUnavailable(
            UnavailableReason::NeedsReconciliation
        ))
    ));

    committed_publish(
        &fixture,
        Some(1),
        "complete-op",
        "reconcile-2",
        &CoverageProjection::Complete {
            paid_intervals: vec![paid("paid", "personal", 0, 30 * DAY)],
        },
    )
    .await
    .expect("publish recovered projection");
    assert_eq!(
        load(&fixture.pool, &fixture.beneficiary_id)
            .await
            .unwrap()
            .revision,
        2
    );
    cleanup(&fixture).await;
}

#[tokio::test]
async fn rollback_leaves_missing_projection_and_retry_can_apply() {
    let Some(fixture) = Fixture::create().await else {
        return;
    };
    let mut tx = fixture.pool.begin().await.expect("begin rollback");
    publish(
        &mut tx,
        &fixture.beneficiary_id,
        None,
        "rolled-back",
        "evidence",
        &CoverageProjection::Complete {
            paid_intervals: vec![paid("paid", "personal", 0, 30 * DAY)],
        },
    )
    .await
    .expect("publish before rollback");
    tx.rollback().await.expect("rollback publication");
    assert!(matches!(
        load(&fixture.pool, &fixture.beneficiary_id).await,
        Err(StoreError::ProjectionMissing)
    ));
    committed_publish(
        &fixture,
        None,
        "rolled-back",
        "evidence",
        &CoverageProjection::Complete {
            paid_intervals: vec![paid("paid", "personal", 0, 30 * DAY)],
        },
    )
    .await
    .expect("retry after rollback");
    cleanup(&fixture).await;
}

#[tokio::test]
async fn renewal_correction_replaces_recovery_without_rewriting_old_revision() {
    let Some(fixture) = Fixture::create().await else {
        return;
    };
    let old = CoverageProjection::Complete {
        paid_intervals: vec![recovery("paid", "personal", 0, 30 * DAY, "renewal-1")],
    };
    committed_publish(&fixture, None, "old", "evidence-old", &old)
        .await
        .expect("publish old recovery");
    let renewed = CoverageProjection::Complete {
        paid_intervals: vec![
            paid("paid", "personal", 0, 30 * DAY),
            paid("renewed", "personal", 30 * DAY, 60 * DAY),
        ],
    };
    let mut rolled_back = fixture
        .pool
        .begin()
        .await
        .expect("begin rolled-back correction");
    publish(
        &mut rolled_back,
        &fixture.beneficiary_id,
        Some(1),
        "rolled-back-correction",
        "evidence-rolled-back-correction",
        &renewed,
    )
    .await
    .expect("publish rolled-back correction");
    rolled_back.rollback().await.expect("rollback correction");
    assert_eq!(
        load(&fixture.pool, &fixture.beneficiary_id)
            .await
            .expect("read revision after rollback")
            .revision,
        1
    );
    let mut pending = fixture.pool.begin().await.expect("begin confirmed renewal");
    publish(
        &mut pending,
        &fixture.beneficiary_id,
        Some(1),
        "renewed",
        "evidence-renewed",
        &renewed,
    )
    .await
    .expect("publish confirmed renewal");
    assert_eq!(
        load(&fixture.pool, &fixture.beneficiary_id)
            .await
            .expect("read old committed revision")
            .revision,
        1
    );
    pending.commit().await.expect("commit confirmed renewal");
    let loaded = load(&fixture.pool, &fixture.beneficiary_id)
        .await
        .expect("load renewed coverage");
    let decision = evaluate(&loaded.coverage, 31 * DAY).unwrap();
    assert_eq!(decision.state, CoverageState::Paid);
    assert_eq!(decision.active_until, Some(60 * DAY));
    assert_eq!(
        evaluate(&loaded.coverage, 60 * DAY).unwrap().export_until,
        Some(90 * DAY)
    );

    let replay = committed_publish(&fixture, None, "old", "evidence-old", &old)
        .await
        .expect("replay old recovery");
    assert_eq!(replay.outcome, PublicationOutcome::AlreadyApplied);
    assert_eq!(
        load(&fixture.pool, &fixture.beneficiary_id)
            .await
            .unwrap()
            .revision,
        2
    );
    cleanup(&fixture).await;
}

#[tokio::test]
async fn future_max_timestamp_can_be_stored_without_eager_export_overflow() {
    let Some(fixture) = Fixture::create().await else {
        return;
    };
    committed_publish(
        &fixture,
        None,
        "future-max",
        "evidence-max",
        &CoverageProjection::Complete {
            paid_intervals: vec![paid("future", "personal", i64::MAX - 1, i64::MAX)],
        },
    )
    .await
    .expect("publish future max interval");
    let loaded = load(&fixture.pool, &fixture.beneficiary_id)
        .await
        .expect("load future max interval");
    assert_eq!(
        evaluate(&loaded.coverage, 5).unwrap().state,
        CoverageState::Free
    );
    cleanup(&fixture).await;
}

#[tokio::test]
async fn stored_fact_count_mismatch_fails_closed() {
    let Some(fixture) = Fixture::create().await else {
        return;
    };
    committed_publish(
        &fixture,
        None,
        "corrupt-count",
        "evidence-corrupt",
        &CoverageProjection::Complete {
            paid_intervals: vec![paid("paid", "personal", 0, 30 * DAY)],
        },
    )
    .await
    .expect("publish projection to corrupt");
    sqlx::query(
        "UPDATE cloud_coverage_revisions SET fact_count = 0 \
         WHERE beneficiary_id = $1 AND revision = 1",
    )
    .bind(&fixture.beneficiary_id)
    .execute(&fixture.pool)
    .await
    .expect("corrupt stored fact count");
    assert!(matches!(
        load(&fixture.pool, &fixture.beneficiary_id).await,
        Err(StoreError::CorruptProjection(_))
    ));
    cleanup(&fixture).await;
}

#[tokio::test]
async fn unavailable_projection_with_stored_facts_fails_closed() {
    let Some(fixture) = Fixture::create().await else {
        return;
    };
    committed_publish(
        &fixture,
        None,
        "corrupt-unavailable",
        "evidence-corrupt-unavailable",
        &CoverageProjection::Unavailable {
            reason: UnavailableReason::NeedsReconciliation,
        },
    )
    .await
    .expect("publish unavailable projection to corrupt");
    sqlx::query(
        "INSERT INTO cloud_coverage_revision_facts \
         (beneficiary_id, revision, coverage_id, source_id, starts_at, paid_until) \
         VALUES ($1, 1, 'unexpected', 'source', 0, 10)",
    )
    .bind(&fixture.beneficiary_id)
    .execute(&fixture.pool)
    .await
    .expect("insert unexpected unavailable fact");
    assert!(matches!(
        load(&fixture.pool, &fixture.beneficiary_id).await,
        Err(StoreError::CorruptProjection(_))
    ));
    cleanup(&fixture).await;
}
