use super::*;

pub(super) async fn exercise(backend: &PostgresBackend, admin: &PgPool) {
    let alice = principal("runtime-concurrency", "alice");
    let bob = principal("runtime-concurrency", "bob");
    let foreign = principal("another-runtime", "alice");
    let store = backend.runtime(alice.tenant_id().clone());
    let first_session = session(backend, &alice, "first").await;
    let other_session = session(backend, &alice, "other").await;
    let bob_session = session(backend, &bob, "bob").await;
    let request = submission(&first_session, "first", 0);
    let (one, two) = tokio::join!(
        store.submit(&alice, request.clone()),
        store.submit(&alice, request.clone())
    );
    let first = one.unwrap();
    assert_eq!(first, two.unwrap());
    assert_eq!(
        store
            .input_version(&alice.owner(), &first_session)
            .await
            .unwrap(),
        1
    );
    let mut mismatch = request.clone();
    mismatch.text = "different".to_owned();
    assert!(matches!(
        store.submit(&alice, mismatch).await,
        Err(ApplicationError::Conflict)
    ));
    assert!(matches!(
        store.submit(&bob, request.clone()).await,
        Err(ApplicationError::NotFound)
    ));
    assert!(matches!(
        store.submit(&foreign, request).await,
        Err(ApplicationError::NotFound)
    ));
    assert!(matches!(
        store
            .submit(&alice, submission(&first_session, "stale", 0))
            .await,
        Err(ApplicationError::Conflict)
    ));
    assert!(matches!(
        store.get(&bob.owner(), &first.id).await,
        Err(ApplicationError::NotFound)
    ));

    let w1 = worker("one");
    let w2 = worker("two");
    let (one, two) = tokio::join!(store.claim(&w1, duration()), store.claim(&w2, duration()));
    let (one, two) = (one.unwrap(), two.unwrap());
    assert_eq!(usize::from(one.is_some()) + usize::from(two.is_some()), 1);
    let claimed = one.or(two).unwrap();
    assert_eq!(claimed.job.id, first.id);
    assert_eq!(claimed.lease.owner, alice.owner());
    let mut wrong = claimed.lease.clone();
    wrong.owner = bob.owner();
    assert!(matches!(
        store.renew(&wrong, duration()).await,
        Err(ApplicationError::LeaseLost)
    ));
    wrong = claimed.lease.clone();
    wrong.worker = worker("forged");
    assert!(matches!(
        store.renew(&wrong, duration()).await,
        Err(ApplicationError::LeaseLost)
    ));
    wrong = claimed.lease.clone();
    wrong.epoch += 1;
    assert!(matches!(
        store.renew(&wrong, duration()).await,
        Err(ApplicationError::LeaseLost)
    ));
    assert!(matches!(
        store.checkpoint(&claimed.lease, checkpoint(&first)).await,
        Err(ApplicationError::Conflict)
    ));
    assert!(matches!(
        store
            .finish(
                &claimed.lease,
                JobFinish::Completed {
                    result: json!("early")
                }
            )
            .await,
        Err(ApplicationError::Conflict)
    ));

    let second = store
        .submit(&alice, submission(&first_session, "second", 1))
        .await
        .unwrap();
    let a2 = store
        .submit(&alice, submission(&other_session, "independent", 0))
        .await
        .unwrap();
    let b = store
        .submit(&bob, submission(&bob_session, "independent", 0))
        .await
        .unwrap();
    let w3 = worker("three");
    let w4 = worker("four");
    let (three, four) = tokio::join!(store.claim(&w3, duration()), store.claim(&w4, duration()));
    let (three, four) = (three.unwrap().unwrap(), four.unwrap().unwrap());
    assert_ne!(three.job.session_id, four.job.session_id);
    assert!([&a2.id, &b.id].contains(&&three.job.id));
    assert!([&a2.id, &b.id].contains(&&four.job.id));
    for independent in [three, four] {
        store
            .finish(
                &independent.lease,
                JobFinish::Cancelled {
                    reason: "fixture complete".to_owned(),
                },
            )
            .await
            .unwrap();
    }
    let renewed = store
        .renew(&claimed.lease, LeaseDuration::new(1_000).unwrap())
        .await
        .unwrap();
    assert!(renewed.expires_at_ms >= claimed.lease.expires_at_ms);
    consume(admin, &first).await;
    let boundary = checkpoint(&first);
    let released = store
        .checkpoint(&claimed.lease, boundary.clone())
        .await
        .unwrap();
    assert_eq!(released.phase, JobPhase::Ready);
    assert_eq!(released.checkpoint_version, 1);
    assert_eq!(released.checkpoint, Some(boundary.clone()));
    let resumed = store
        .claim(&worker("replacement"), duration())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(resumed.job.id, first.id);
    assert_eq!(resumed.job.checkpoint, Some(boundary));
    assert!(resumed.lease.epoch > claimed.lease.epoch);
    assert_ne!(resumed.lease.attempt_id, claimed.lease.attempt_id);
    assert!(matches!(
        store
            .finish(
                &claimed.lease,
                JobFinish::Completed {
                    result: json!("stale")
                }
            )
            .await,
        Err(ApplicationError::LeaseLost)
    ));
    let finished = store
        .finish(
            &resumed.lease,
            JobFinish::Completed {
                result: json!({"completed":true}),
            },
        )
        .await
        .unwrap();
    assert_eq!(finished.phase, JobPhase::Completed);
    assert_eq!(finished.result, Some(json!({"completed":true})));
    let next = store
        .claim(&worker("next-turn"), duration())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(next.job.id, second.id);
    store
        .finish(
            &next.lease,
            JobFinish::Cancelled {
                reason: "fixture complete".to_owned(),
            },
        )
        .await
        .unwrap();

    let mut tx = scoped_transaction(&backend.pool, &bob).await.unwrap();
    assert_eq!(
        query_scalar::<_, i64>(
            "SELECT count(*) FROM zuno_enterprise_preview.runtime_job WHERE principal_id='alice'"
        )
        .fetch_one(&mut *tx)
        .await
        .unwrap(),
        0
    );
    tx.rollback().await.unwrap();
    assert_eq!(
        query_scalar::<_, i64>("SELECT count(*) FROM zuno_enterprise_preview.runtime_job")
            .fetch_one(&backend.pool)
            .await
            .unwrap(),
        0
    );
}
