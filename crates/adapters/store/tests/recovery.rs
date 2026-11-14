//! What the SQLite store does to a log a stopped daemon left behind: the
//! running operations end failed with `daemon-stopped`, in the columns and
//! across opens of the same file, a failure part way through ends none, and
//! the queued ones are handed back as they were, to run again (decision 73).

mod support;

use std::assert_matches;

use app::ports::{OperationDetail, OperationRecord, OperationStore, Start, StoreError};
use domain::execution::{FailureReason, OperationKind, OperationStatus};
use support::*;

/// How many running operations recovery ended, or why it couldn't.
async fn ended(store: &store::SqliteStore) -> Result<usize, StoreError> {
    store
        .recover(&FailureReason::DaemonStopped)
        .await
        .map(|recovered| recovered.ended.len())
}

fn daemon_stopped() -> OperationStatus {
    OperationStatus::Failed {
        reason: FailureReason::DaemonStopped,
    }
}

#[tokio::test]
async fn an_operation_a_closed_store_left_running_is_ended_and_a_queued_one_handed_back() {
    let scratch = Scratch::new();
    let running = with_warnings(started(queued_backup(0x10)), 2);
    let waiting = queued_backup(0x11);

    let first = scratch.open().await;
    save(&first, &running, None).await;
    save(&first, &waiting, None).await;
    first.close().await;

    let ended = |op: &domain::execution::Operation| OperationDetail {
        record: OperationRecord {
            status: daemon_stopped(),
            finished_at: None,
            ..OperationRecord::of(op, None)
        },
        warnings: op.warnings().to_vec(),
    };

    let second = scratch.open().await;
    let recovered = within(
        30,
        "recovery",
        second.recover(&FailureReason::DaemonStopped),
    )
    .await
    .expect("recovery works");
    assert_eq!(
        recovered.ended,
        vec![running.id()],
        "the running operation is ended"
    );
    assert_eq!(
        recovered.queued,
        vec![OperationRecord::of(&waiting, None)],
        "the queued one is handed back as it was queued"
    );
    assert_eq!(recovered.unreadable, Vec::new());
    assert_eq!(second.get(running.id()).await, Ok(Some(ended(&running))));
    assert_eq!(
        second.get(waiting.id()).await,
        Ok(Some(detail_of(&waiting, None))),
        "the queued operation is still queued"
    );
    second.close().await;

    let third = scratch.open().await;
    assert_eq!(
        third.get(running.id()).await,
        Ok(Some(ended(&running))),
        "the running operation is still failed when the file is opened again"
    );
    let again = third
        .recover(&FailureReason::DaemonStopped)
        .await
        .expect("recovery works");
    assert_eq!(again.ended, Vec::new(), "nothing is left to end");
    assert_eq!(again.queued, vec![OperationRecord::of(&waiting, None)]);
    third.close().await;
}

#[tokio::test]
async fn queued_operations_come_back_oldest_first_and_an_unreadable_one_is_reported() {
    let scratch = Scratch::new();
    let store = scratch.open().await;
    let mut raw = scratch.raw().await;
    save(&store, &queued_backup(0x22), None).await;
    save(&store, &queued_backup(0x20), None).await;
    save(&store, &queued_backup(0x21), None).await;
    save(&store, &succeeded(queued_backup(0x23)), None).await;
    // A trigger this build doesn't know, as a later version might write.
    sqlx::query("UPDATE operations SET trigger = 'teleport' WHERE id = ?")
        .bind(id_text(0x21))
        .execute(&mut raw)
        .await
        .expect("the row is changed");
    let before = stored_row(&mut raw, 0x21).await;

    let recovered = store
        .recover(&FailureReason::DaemonStopped)
        .await
        .expect("an unreadable queued row doesn't refuse the start");
    assert_eq!(
        recovered.queued,
        vec![
            OperationRecord::of(&queued_backup(0x20), None),
            OperationRecord::of(&queued_backup(0x22), None),
        ],
        "oldest first, which is ID order"
    );
    assert_eq!(recovered.unreadable.len(), 1, "{recovered:?}");
    assert_eq!(
        stored_row(&mut raw, 0x21).await,
        before,
        "the unreadable row is left as it was"
    );
    store.close().await;
}

#[tokio::test]
async fn ended_operations_come_back_oldest_first_and_an_unreadable_one_is_ended_and_reported() {
    let scratch = Scratch::new();
    let store = scratch.open().await;
    let mut raw = scratch.raw().await;
    // Stored newest first, so the table's order isn't the answer's.
    save(&store, &started(queued_backup(0x3c)), None).await;
    save(&store, &started(queued_backup(0x30)), None).await;
    save(&store, &started(queued_backup(0x3b)), None).await;
    // An ID this build doesn't read: not in lowercase hyphenated form.
    let unreadable = id_text(0x3b).to_uppercase();
    sqlx::query("UPDATE operations SET id = ? WHERE id = ?")
        .bind(&unreadable)
        .bind(id_text(0x3b))
        .execute(&mut raw)
        .await
        .expect("the row is changed");

    let recovered = store
        .recover(&FailureReason::DaemonStopped)
        .await
        .expect("an unreadable running row doesn't refuse the start");
    assert_eq!(
        recovered.ended,
        vec![queued_backup(0x30).id(), queued_backup(0x3c).id()],
        "oldest first, which is ID order"
    );
    assert_eq!(recovered.unreadable.len(), 1, "{recovered:?}");
    let status: String = sqlx::query_scalar("SELECT status FROM operations WHERE id = ?")
        .bind(&unreadable)
        .fetch_one(&mut raw)
        .await
        .expect("the row is read");
    assert_eq!(status, "failed", "the unreadable row is ended all the same");
    store.close().await;
}

#[tokio::test]
async fn recovery_changes_only_the_status_and_failure_columns_of_running_rows() {
    let scratch = Scratch::new();
    let store = scratch.open().await;
    let mut raw = scratch.raw().await;

    let mut cancelled = started(queued_backup(0x15));
    cancelled
        .cancel(at(40))
        .expect("a running operation cancels");
    let mut warned = with_warnings(started(queued_backup(0x16)), 1);
    warned
        .succeed(at(70))
        .expect("a running operation succeeds");

    save(
        &store,
        &with_warnings(started(queued_backup(0x10)), 2),
        None,
    )
    .await;
    save(&store, &queued_backup(0x11), None).await;
    save(
        &store,
        &succeeded(queued_backup(0x12)),
        Some(&backup_result()),
    )
    .await;
    save(
        &store,
        &failed(
            queued_backup(0x13),
            FailureReason::EngineFailed {
                detail: "disk full".to_owned(),
            },
        ),
        None,
    )
    .await;
    save(
        &store,
        &failed(
            queued_backup(0x14),
            FailureReason::EngineExited { exit_code: Some(7) },
        ),
        None,
    )
    .await;
    save(&store, &cancelled, None).await;
    save(&store, &warned, None).await;

    let running = [0x10];
    let unchanged = [0x11, 0x12, 0x13, 0x14, 0x15, 0x16];
    let mut rows_before = Vec::new();
    let mut warnings_before = Vec::new();
    for n in running.into_iter().chain(unchanged) {
        rows_before.push((n, stored_row(&mut raw, n).await));
        warnings_before.push((n, stored_warnings(&mut raw, n).await));
    }

    assert_eq!(ended(&store).await, Ok(1));

    for ((n, before), (_, warnings)) in rows_before.into_iter().zip(warnings_before) {
        let after = stored_row(&mut raw, n).await;
        if running.contains(&n) {
            assert!(
                before.status == "running",
                "operation {n:#x} was running: {before:?}"
            );
            let expected = Stored {
                status: "failed".to_owned(),
                error_code: Some("daemon-stopped".to_owned()),
                error_args: Some("[]".to_owned()),
                error_detail: None,
                ..before
            };
            assert_eq!(
                after, expected,
                "only the status and failure columns of {n:#x} changed"
            );
        } else {
            assert_eq!(after, before, "queued or finished row {n:#x} is identical");
        }
        assert_eq!(
            stored_warnings(&mut raw, n).await,
            warnings,
            "the warnings of {n:#x} are as they were"
        );
    }
}

#[tokio::test]
async fn a_failure_part_way_through_leaves_every_row_as_it_was() {
    let scratch = Scratch::new();
    let store = scratch.open().await;
    let mut raw = scratch.raw().await;

    save(&store, &queued_backup(0x10), None).await;
    save(
        &store,
        &with_warnings(started(queued_backup(0x11)), 1),
        None,
    )
    .await;
    save(&store, &queued_backup(0x12), None).await;
    save(&store, &started(queued_backup(0x13)), None).await;
    save(
        &store,
        &started(queued_on_repository(0x14, OperationKind::Prune)),
        None,
    )
    .await;
    save(&store, &queued_backup(0x15), None).await;
    save(&store, &succeeded(queued_backup(0x16)), None).await;

    let numbers = [0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16];
    let mut rows_before = Vec::new();
    let mut seen_before = Vec::new();
    for n in numbers {
        rows_before.push(stored_row(&mut raw, n).await);
        seen_before.push(store.get(id(n)).await.expect("reading works"));
    }

    // The update of one unfinished row in the middle of the log fails, as a
    // full disk would at that point.
    let refuse = format!(
        "CREATE TRIGGER refuse_poisoned BEFORE UPDATE ON operations
         WHEN NEW.id = '{}'
         BEGIN SELECT RAISE(ABORT, 'boom'); END",
        id_text(0x13)
    );
    sqlx::query(sqlx::AssertSqlSafe(refuse))
        .execute(&mut raw)
        .await
        .expect("the fault is set up");

    let outcome = ended(&store).await;
    assert_matches!(outcome, Err(StoreError::Unavailable { .. }));

    for ((n, before), seen) in numbers.into_iter().zip(rows_before).zip(seen_before) {
        assert_eq!(
            stored_row(&mut raw, n).await,
            before,
            "row {n:#x} is as it was"
        );
        assert_eq!(
            store.get(id(n)).await.expect("reading works"),
            seen,
            "operation {n:#x} reads as it did"
        );
    }
}

#[tokio::test]
async fn recovery_frees_the_repository_a_running_maintenance_operation_held_across_the_unique_index()
 {
    let scratch = Scratch::new();
    let store = scratch.open().await;

    let holder = started(queued_on_repository(0x10, OperationKind::Prune));
    assert_eq!(store.try_start(&holder).await, Ok(Start::Started));
    let blocked = store
        .save(
            &started(queued_on_repository(0x11, OperationKind::Prune)),
            None,
        )
        .await;
    assert_matches!(
        blocked,
        Err(StoreError::Unavailable { .. }),
        "the unique index holds the repository while the prune runs: {blocked:?}"
    );

    assert_eq!(ended(&store).await, Ok(1));

    let next = started(queued_on_repository(0x12, OperationKind::Prune));
    save(&store, &next, None).await;
    assert_eq!(
        store.get(id(0x12)).await,
        Ok(Some(detail_of(&next, None))),
        "a running prune is stored once the old one is ended"
    );
    assert_eq!(
        store
            .try_start(&started(queued_on_repository(0x13, OperationKind::Check)))
            .await,
        Ok(Start::Refused { running: id(0x12) }),
        "and it holds the repository in its turn"
    );
}
