//! The operation store's contract for ending what a stopped daemon left
//! behind: every running operation ends failed with `DaemonStopped`, a
//! running maintenance operation stops holding its repository (I7), and the
//! queued ones stay queued and are handed back, oldest first, for the
//! daemon to run again (decision 73). Each case seeds operations and
//! repositories of its own, recovers, and leaves nothing unfinished behind,
//! so the next case's count is exact. The first case needs the log to hold
//! nothing unfinished.

use domain::execution::OperationKind::{Adopt, Backup, Check, Forget, Prune};
use domain::execution::{
    FailureReason, Operation, OperationKind, OperationStatus, Target, Trigger,
};
use domain::{OperationId, PlanId, RepositoryId};

use super::{applied, at, failed, id, listing, queued, read, save, started, succeeded, unreadable};
use crate::ports::{
    EngineResult, OperationDetail, OperationRecord, OperationStore, Recovered, Start,
};

pub(super) async fn run(store: &dyn OperationStore) {
    an_empty_log_recovers_nothing(store).await;
    a_running_operation_ends_failed_and_keeps_all_else(store).await;
    a_queued_operation_stays_queued_and_is_handed_back(store).await;
    finished_operations_are_untouched(store).await;
    each_ended_operation_is_named_and_a_second_recovery_finds_nothing(store).await;
    a_running_maintenance_operation_stops_holding_its_repository(store).await;
    recovery_frees_only_what_maintenance_held(store).await;
    nothing_unfinished_ends_succeeded(store).await;
}

fn daemon_stopped() -> OperationStatus {
    OperationStatus::Failed {
        reason: FailureReason::DaemonStopped,
    }
}

/// The repository numbered `n`: 64 hex digits, `n`'s two digits over again.
fn numbered(n: u8) -> Option<RepositoryId> {
    let parsed = RepositoryId::parse(&format!("{n:02x}").repeat(32));
    assert!(
        parsed.is_ok(),
        "the suite's own repository IDs are valid: {parsed:?}"
    );
    parsed.ok()
}

fn plan(name: &str) -> Option<PlanId> {
    let parsed = PlanId::parse(name);
    assert!(
        parsed.is_ok(),
        "the suite's own plan IDs are valid: {parsed:?}"
    );
    parsed.ok()
}

/// A queued operation of `kind` on `repository`, or on none.
fn on(n: u128, kind: OperationKind, repository: Option<&RepositoryId>) -> Operation {
    let target = Target {
        plan: None,
        repository: repository.cloned(),
    };
    Operation::queue(id(n), kind, Trigger::Manual, target, at(0))
}

/// What the log holds for `op`: its record and its warnings.
fn stored(op: &Operation) -> OperationDetail {
    OperationDetail {
        record: OperationRecord::of(op, None),
        warnings: op.warnings().to_vec(),
    }
}

/// Stores `op` as running through `try_start`, which must accept it.
async fn start_running(store: &dyn OperationStore, op: &Operation) {
    let decided = store.try_start(op).await;
    assert_eq!(
        decided,
        Ok(Start::Started),
        "operation {} starts",
        op.id().as_uuid()
    );
}

/// What a start of `op` decides; a store that can't answer fails the suite.
async fn try_start(store: &dyn OperationStore, op: &Operation) -> Option<Start> {
    let decided = store.try_start(op).await;
    assert!(
        decided.is_ok(),
        "starting operation {} failed: {decided:?}",
        op.id().as_uuid()
    );
    decided.ok()
}

/// How many operations recovery ended; a store that can't recover fails the
/// suite.
async fn recover(store: &dyn OperationStore) -> Option<usize> {
    recovered(store)
        .await
        .map(|recovered| recovered.ended.len())
}

/// What recovery did and found; a store that can't recover fails the suite.
async fn recovered(store: &dyn OperationStore) -> Option<Recovered> {
    let recovered = store.recover(&FailureReason::DaemonStopped).await;
    assert!(recovered.is_ok(), "recovering failed: {recovered:?}");
    recovered.ok()
}

/// The IDs of the queued operations recovery hands back, in its order.
async fn queued_ids(store: &dyn OperationStore) -> Option<Vec<OperationId>> {
    recovered(store)
        .await
        .map(|recovered| recovered.queued.iter().map(|record| record.id).collect())
}

/// Cancels every queued operation, so the cases that follow find none.
async fn clear_queue(store: &dyn OperationStore) {
    let Some(recovered) = recovered(store).await else {
        return;
    };
    for record in recovered.queued {
        let op = record.queued_operation();
        assert!(
            op.is_some(),
            "recovery handed back an operation that isn't queued: {record:?}"
        );
        let Some(mut op) = op else {
            return;
        };
        applied(op.cancel(at(90)));
        save(store, &op, None).await;
    }
}

async fn detail(store: &dyn OperationStore, n: u128) -> Option<OperationDetail> {
    let found = read(store, id(n)).await;
    assert!(found.is_some(), "operation {n:#x} is in the log");
    found
}

async fn an_empty_log_recovers_nothing(store: &dyn OperationStore) {
    assert_eq!(
        recover(store).await,
        Some(0),
        "an empty log has nothing to end"
    );
    assert_eq!(
        listing(store, None, 10).await,
        Vec::new(),
        "and recovering it adds nothing"
    );
}

async fn a_running_operation_ends_failed_and_keeps_all_else(store: &dyn OperationStore) {
    let (Some(home), Some(repository)) = (plan("demo-recover-home"), numbered(0x70)) else {
        return;
    };
    let target = Target {
        plan: Some(home),
        repository: Some(repository),
    };
    let mut op = started(Operation::queue(
        id(0x7000),
        Backup,
        Trigger::CatchUp,
        target.clone(),
        at(3),
    ));
    applied(op.record_warning(unreadable("/home/demo/a")));
    applied(op.record_warning(unreadable("/home/demo/b")));
    start_running(store, &op).await;
    let before = detail(store, 0x7000).await;
    assert_eq!(before, Some(stored(&op)), "the backup is stored running");

    assert_eq!(recover(store).await, Some(1));

    let after = detail(store, 0x7000).await;
    assert!(after.is_some());
    let (Some(before), Some(after)) = (before, after) else {
        return;
    };
    let record = after.record.clone();
    assert_eq!(
        record.status,
        daemon_stopped(),
        "a running backup ends failed: daemon-stopped"
    );
    assert_eq!(
        record.finished_at, None,
        "when it really ended is unknown, so no end time is made up"
    );
    assert_eq!(record.started_at, Some(at(10)), "it keeps when it started");
    assert_eq!(record.queued_at, at(3), "it keeps when it was queued");
    assert_eq!(record.kind, Backup);
    assert_eq!(record.trigger, Trigger::CatchUp);
    assert_eq!(record.target, target);
    assert_eq!(record.warning_count, 2);
    assert_eq!(after.warnings, op.warnings(), "it keeps its warnings");
    assert_eq!(
        after,
        OperationDetail {
            record: OperationRecord {
                status: daemon_stopped(),
                finished_at: None,
                ..before.record
            },
            warnings: before.warnings,
        },
        "nothing but its status changed"
    );
}

async fn a_queued_operation_stays_queued_and_is_handed_back(store: &dyn OperationStore) {
    let (Some(docs), Some(repository)) = (plan("demo-recover-docs"), numbered(0x73)) else {
        return;
    };
    let newer = queued(0x7101, Some(&docs));
    save(store, &newer, None).await;
    let older = Operation::queue(
        id(0x7100),
        Backup,
        Trigger::Schedule,
        Target {
            plan: Some(docs.clone()),
            repository: Some(repository),
        },
        at(1),
    );
    save(store, &older, None).await;

    let found = recovered(store).await;
    assert_eq!(
        found,
        Some(Recovered {
            ended: Vec::new(),
            queued: vec![
                OperationRecord::of(&older, None),
                OperationRecord::of(&newer, None)
            ],
            unreadable: Vec::new(),
        }),
        "a queued operation isn't ended: it comes back as it was queued, oldest first"
    );
    assert_eq!(
        detail(store, 0x7101).await,
        Some(stored(&newer)),
        "and the log still shows it queued"
    );
    assert_eq!(
        queued_ids(store).await,
        Some(vec![id(0x7100), id(0x7101)]),
        "a second recovery hands it back again"
    );
    clear_queue(store).await;
    assert_eq!(queued_ids(store).await, Some(Vec::new()));
}

async fn finished_operations_are_untouched(store: &dyn OperationStore) {
    let Some(photos) = plan("demo-recover-photos") else {
        return;
    };
    let result = EngineResult::Fake { steps_completed: 3 };
    save(
        store,
        &succeeded(queued(0x7200, Some(&photos))),
        Some(&result),
    )
    .await;

    let mut warned = started(queued(0x7201, Some(&photos)));
    applied(warned.record_warning(unreadable("/home/demo/w")));
    applied(warned.succeed(at(70)));
    save(store, &warned, None).await;

    save(
        store,
        &failed(
            queued(0x7202, Some(&photos)),
            FailureReason::EngineFailed {
                detail: "disk full".to_owned(),
            },
        ),
        None,
    )
    .await;
    save(
        store,
        &failed(
            queued(0x7203, Some(&photos)),
            FailureReason::EngineExited { exit_code: Some(7) },
        ),
        None,
    )
    .await;

    let mut cancelled = started(queued(0x7204, Some(&photos)));
    applied(cancelled.cancel(at(40)));
    save(store, &cancelled, None).await;

    let mut before = Vec::new();
    for n in 0x7200..=0x7204 {
        let Some(found) = detail(store, n).await else {
            return;
        };
        assert!(
            found.record.status.is_finished() && found.record.finished_at.is_some(),
            "operation {n:#x} is finished before recovery: {found:?}"
        );
        before.push((n, found));
    }
    assert_eq!(
        before.first().map(|(_, found)| found.record.result.clone()),
        Some(Some(result)),
        "the succeeded operation keeps its engine result"
    );

    assert_eq!(
        recover(store).await,
        Some(0),
        "finished operations are not ended"
    );

    for (n, was) in before {
        assert_eq!(
            detail(store, n).await,
            Some(was),
            "finished operation {n:#x} is as it was, bit for bit"
        );
    }
}

async fn each_ended_operation_is_named_and_a_second_recovery_finds_nothing(
    store: &dyn OperationStore,
) {
    let (Some(repo_a), Some(repo_b), Some(music)) =
        (numbered(0x71), numbered(0x72), plan("demo-recover-music"))
    else {
        return;
    };
    // Unfinished: two queued and two running backups, a queued prune, a
    // running prune and a running check. Only the running ones end.
    save(store, &queued(0x7300, Some(&music)), None).await;
    save(store, &queued(0x7301, None), None).await;
    start_running(store, &started(queued(0x7302, Some(&music)))).await;
    start_running(store, &started(queued(0x7303, None))).await;
    save(store, &on(0x7304, Prune, Some(&repo_a)), None).await;
    start_running(store, &started(on(0x7305, Prune, Some(&repo_b)))).await;
    start_running(store, &started(on(0x7306, Check, Some(&repo_a)))).await;
    // Finished: not counted.
    save(store, &succeeded(queued(0x7310, Some(&music))), None).await;
    save(
        store,
        &failed(
            on(0x7311, Prune, Some(&repo_a)),
            FailureReason::EngineExited { exit_code: None },
        ),
        None,
    )
    .await;

    assert_eq!(
        recovered(store).await.map(|found| found.ended),
        Some(vec![id(0x7302), id(0x7303), id(0x7305), id(0x7306)]),
        "every running operation is ended, and only those, oldest first"
    );
    for n in [0x7302, 0x7303, 0x7305, 0x7306] {
        assert_eq!(
            detail(store, n).await.map(|found| found.record.status),
            Some(daemon_stopped()),
            "operation {n:#x} ended failed"
        );
    }
    assert_eq!(
        queued_ids(store).await,
        Some(vec![id(0x7300), id(0x7301), id(0x7304)]),
        "the queued ones stay queued"
    );

    let log = listing(store, None, 10).await;
    assert_eq!(
        recover(store).await,
        Some(0),
        "a second recovery has nothing left to end"
    );
    assert_eq!(listing(store, None, 10).await, log, "and changes nothing");
    clear_queue(store).await;
}

async fn a_running_maintenance_operation_stops_holding_its_repository(store: &dyn OperationStore) {
    for (number, kind) in (0_u8..).zip([Prune, Check, Forget, Adopt]) {
        let Some(repository) = numbered(0x60 + number) else {
            return;
        };
        let n = 0x7400 + 0x10 * u128::from(number);
        let holder = started(on(n, kind, Some(&repository)));
        start_running(store, &holder).await;

        let other_kind = if kind == Prune { Check } else { Prune };
        let next = started(on(n + 1, other_kind, Some(&repository)));
        assert_eq!(
            try_start(store, &next).await,
            Some(Start::Refused { running: id(n) }),
            "a running {kind:?} holds its repository before recovery"
        );

        assert_eq!(recover(store).await, Some(1), "the {kind:?} is ended");
        assert_eq!(
            detail(store, n).await.map(|found| found.record.status),
            Some(daemon_stopped())
        );
        assert_eq!(
            read(store, id(n + 1)).await,
            None,
            "the refused start stored nothing"
        );
        assert_eq!(
            try_start(store, &next).await,
            Some(Start::Started),
            "the repository is free once the {kind:?} is ended"
        );

        // Leave nothing unfinished for the cases that follow.
        assert_eq!(recover(store).await, Some(1));
    }
}

async fn recovery_frees_only_what_maintenance_held(store: &dyn OperationStore) {
    let (Some(repo_a), Some(repo_b), Some(repo_c)) =
        (numbered(0x68), numbered(0x69), numbered(0x6a))
    else {
        return;
    };
    start_running(store, &started(on(0x7500, Prune, Some(&repo_a)))).await;
    assert_eq!(
        try_start(store, &started(on(0x7501, Prune, Some(&repo_b)))).await,
        Some(Start::Started),
        "maintenance on repository B isn't held up by repository A's"
    );
    // A running backup names a repository but never holds it.
    let backup = Operation::queue(
        id(0x7502),
        Backup,
        Trigger::Manual,
        Target {
            plan: None,
            repository: Some(repo_c.clone()),
        },
        at(0),
    );
    start_running(store, &started(backup)).await;
    assert_eq!(
        try_start(store, &started(on(0x7503, Prune, Some(&repo_c)))).await,
        Some(Start::Started),
        "a running backup holds no repository"
    );

    assert_eq!(recover(store).await, Some(4));

    for (n, repository) in [(0x7510, &repo_a), (0x7511, &repo_b), (0x7512, &repo_c)] {
        assert_eq!(
            try_start(store, &started(on(n, Check, Some(repository)))).await,
            Some(Start::Started),
            "a fresh maintenance operation starts on repository {repository:?}"
        );
    }
    assert_eq!(recover(store).await, Some(3));
}

async fn nothing_unfinished_ends_succeeded(store: &dyn OperationStore) {
    let Some(repository) = numbered(0x6b) else {
        return;
    };
    let running: [OperationId; 2] = [id(0x7601), id(0x7603)];
    save(store, &queued(0x7600, None), None).await;
    start_running(store, &started(queued(0x7601, None))).await;
    save(store, &on(0x7602, Check, Some(&repository)), None).await;
    start_running(store, &started(on(0x7603, Forget, Some(&repository)))).await;
    save(store, &succeeded(queued(0x7610, None)), None).await;

    let succeeded_before = succeeded_ids(store).await;
    assert_eq!(recover(store).await, Some(2));
    clear_queue(store).await;

    for unfinished in running {
        let status = read(store, unfinished)
            .await
            .map(|found| found.record.status);
        assert_eq!(
            status,
            Some(daemon_stopped()),
            "operation {} ended failed, not succeeded",
            unfinished.as_uuid()
        );
    }
    assert_eq!(
        succeeded_ids(store).await,
        succeeded_before,
        "recovery marked nothing succeeded"
    );
}

/// The operations the log shows as succeeded, with or without warnings.
async fn succeeded_ids(store: &dyn OperationStore) -> Vec<OperationId> {
    listing(store, None, 10)
        .await
        .into_iter()
        .filter(|record| {
            matches!(
                record.status,
                OperationStatus::Succeeded | OperationStatus::SucceededWithWarnings
            )
        })
        .map(|record| record.id)
        .collect()
}
