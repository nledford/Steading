//! Steps for `features/execution/recovery.feature`: they leave operations in
//! the in-memory operation log the world's `store` holds, as a daemon that
//! stopped would, start the daemon again through `RecoverOperations` (its
//! recovery, then the persistent queue's resume over a configuration with
//! every plan the scenario names, less those it removed, each on the
//! repository "nas" unless the scenario moved it), and read the log back
//! through its port, and what the queue ran again and said off a recording.
//! What the start announces is told at once to the notification rules of
//! `steps/notifications.rs`, which read the operation from the same log.

use std::assert_matches;
use std::sync::Arc;

use api::UiEvent;
use app::fakes::InMemoryOperationStore;
use app::notifications::Notify;
use app::ports::{
    Clock, EngineResult, EventBus, HistoryPage, HistoryQuery, OperationDetail, OperationStore,
    Start, StoreError,
};
use app::recovery::{QueuedEnd, RecoverOperations};
use app::schedule::ScheduleReport;
use async_trait::async_trait;
use cucumber::{given, then, when};
use domain::execution::{
    FailureReason, Operation, OperationKind, OperationStatus, Target, Trigger, Warning, WarningKind,
};
use domain::{OperationId, PlanId, RepositoryAlias, RepositoryId, Timestamp, Uuid};
use futures::stream::BoxStream;

use super::Agent;
use super::scheduler_steps::{RecordingRuns, config_with_plans};

/// An operation the scenario put in the log, and how it names its target.
struct Seeded {
    id: OperationId,
    kind: OperationKind,
    plan: Option<String>,
    repository: Option<String>,
    /// The log's entry for it as it ended, for an operation that had ended
    /// before the daemon started again.
    ended: Option<OperationDetail>,
}

/// What the scenario left in the log, and what each start of the daemon
/// reported.
#[derive(Default)]
pub struct RecoveryWorld {
    /// How many operations the scenario has made, probes included.
    made: u128,
    seeded: Vec<Seeded>,
    /// What each start of the daemon reported, oldest first.
    starts: Vec<Result<usize, StoreError>>,
    /// The plans the configuration no longer has.
    removed: Vec<String>,
    /// The plans the configuration now has on another repository, and the
    /// alias of that repository.
    moved: Vec<(String, String)>,
    /// Whether the configuration can't be read.
    unreadable: bool,
    /// What the last start ran again.
    runs: Option<Arc<RecordingRuns>>,
}

impl RecoveryWorld {
    fn next_id(&mut self) -> OperationId {
        self.made += 1;
        OperationId::from_uuid(Uuid::from_u128(0x7ec0_0000 + self.made))
    }

    /// A new queued operation of `kind` on the plan or repository the
    /// scenario names.
    fn queue(
        &mut self,
        kind: OperationKind,
        plan: Option<&str>,
        repository: Option<&str>,
    ) -> Operation {
        Operation::queue(
            self.next_id(),
            kind,
            Trigger::Schedule,
            Target {
                plan: plan.map(plan_id),
                repository: repository.map(repository_id),
            },
            at(0),
        )
    }

    fn remember(
        &mut self,
        op: &Operation,
        plan: Option<&str>,
        repository: Option<&str>,
        ended: Option<OperationDetail>,
    ) {
        self.seeded.push(Seeded {
            id: op.id(),
            kind: op.kind(),
            plan: plan.map(str::to_owned),
            repository: repository.map(str::to_owned),
            ended,
        });
    }

    fn backup(&self, plan: &str) -> &Seeded {
        self.seeded
            .iter()
            .find(|seeded| {
                seeded.kind == OperationKind::Backup && seeded.plan.as_deref() == Some(plan)
            })
            .unwrap_or_else(|| {
                panic!("the scenario never put a backup of the plan {plan} in the log")
            })
    }

    fn maintenance(&self, kind: OperationKind, repository: &str) -> &Seeded {
        self.seeded
            .iter()
            .find(|seeded| seeded.kind == kind && seeded.repository.as_deref() == Some(repository))
            .unwrap_or_else(|| {
                panic!(
                    "the scenario never put a {kind:?} of the repository {repository} in the log"
                )
            })
    }

    fn last_start(&self) -> &Result<usize, StoreError> {
        self.starts
            .last()
            .expect("the daemon started again in the scenario")
    }
}

fn at(seconds: i64) -> Timestamp {
    Timestamp::from_second(1_800_000_000 + seconds).expect("a time in range")
}

fn plan_id(name: &str) -> PlanId {
    PlanId::parse(name).expect("the scenario's plan ID is valid")
}

/// The repository this module calls `alias`: always the same 64 hex digits
/// for the same name, as every repository's ID is.
fn repository_id(alias: &str) -> RepositoryId {
    let folded = alias.bytes().fold(0x2ec0_u128, |acc, byte| {
        acc.wrapping_mul(257).wrapping_add(u128::from(byte))
    });
    RepositoryId::parse(&format!("{folded:064x}")).expect("64 hex digits are a repository ID")
}

fn maintenance_kind(name: &str) -> OperationKind {
    match name {
        "prune" => OperationKind::Prune,
        "check" => OperationKind::Check,
        "forget" => OperationKind::Forget,
        "adopt" => OperationKind::Adopt,
        other => panic!("the scenario names a maintenance operation that doesn't exist: {other}"),
    }
}

fn warning() -> Warning {
    Warning {
        kind: WarningKind::SourceUnreadable,
        item: Some("/home/demo/locked.txt".to_owned()),
        during: "backup".to_owned(),
        detail: "permission denied".to_owned(),
    }
}

/// How an operation that ended before the daemon stopped is shown.
fn ending_status(ending: &str) -> OperationStatus {
    match ending {
        "succeeded" => OperationStatus::Succeeded,
        "succeeded with warnings" => OperationStatus::SucceededWithWarnings,
        "failed because the repository was unreachable" => OperationStatus::Failed {
            reason: unreachable_repository(),
        },
        "cancelled" => OperationStatus::Cancelled,
        other => panic!("the scenario ends an operation in a way that doesn't exist: {other}"),
    }
}

fn unreachable_repository() -> FailureReason {
    FailureReason::EngineFailed {
        detail: "repository unreachable".to_owned(),
    }
}

/// Starts `op` on the log and checks the log took it as running.
async fn start(agent: &mut Agent, op: &mut Operation) {
    op.start(at(10)).expect("a queued operation starts");
    let decided = agent
        .store
        .try_start(op)
        .await
        .expect("the log answers a start");
    assert_eq!(
        decided,
        Start::Started,
        "the scenario starts its operation on a free repository"
    );
}

async fn detail(agent: &Agent, id: OperationId) -> OperationDetail {
    agent
        .store
        .get(id)
        .await
        .expect("the log answers")
        .expect("the log holds the operation")
}

#[given(expr = "the log shows a backup of the plan {string} as running")]
async fn backup_running(agent: &mut Agent, plan: String) {
    let mut op = agent
        .recovery
        .queue(OperationKind::Backup, Some(&plan), None);
    start(agent, &mut op).await;
    agent.recovery.remember(&op, Some(&plan), None, None);
}

#[given(
    expr = "the log shows a backup of the plan {string} as running with {int} warning(s) recorded"
)]
async fn backup_running_with_warnings(agent: &mut Agent, plan: String, count: usize) {
    let mut op = agent
        .recovery
        .queue(OperationKind::Backup, Some(&plan), None);
    op.start(at(10)).expect("a queued operation starts");
    for _ in 0..count {
        op.record_warning(warning())
            .expect("a running operation records a warning");
    }
    agent
        .store
        .save(&op, None)
        .await
        .expect("the log answers a save");
    agent.recovery.remember(&op, Some(&plan), None, None);
}

#[given(expr = "the log shows a backup of the plan {string} as queued")]
async fn backup_queued(agent: &mut Agent, plan: String) {
    let op = agent
        .recovery
        .queue(OperationKind::Backup, Some(&plan), None);
    agent
        .store
        .save(&op, None)
        .await
        .expect("the log answers a save");
    agent.recovery.remember(&op, Some(&plan), None, None);
}

#[given(expr = "the log shows a backup of the plan {string} on the repository {string} as queued")]
async fn backup_queued_on(agent: &mut Agent, plan: String, repository: String) {
    let op = agent
        .recovery
        .queue(OperationKind::Backup, Some(&plan), Some(&repository));
    agent
        .store
        .save(&op, None)
        .await
        .expect("the log answers a save");
    agent
        .recovery
        .remember(&op, Some(&plan), Some(&repository), None);
}

#[given(expr = "the log shows a {word} of the repository {string} as running")]
async fn maintenance_running(agent: &mut Agent, kind: String, repository: String) {
    let kind = maintenance_kind(&kind);
    // By the alias the configuration gives the repository, as a maintenance
    // operation the daemon runs is, so its news names it.
    let mut op = agent
        .recovery
        .queue(kind, None, Some(&repository))
        .with_repository_alias(
            RepositoryAlias::parse(&repository).expect("the scenario's alias is valid"),
        );
    start(agent, &mut op).await;
    agent.recovery.remember(&op, None, Some(&repository), None);

    // The repository is held: a maintenance operation of another kind that
    // tries to start on it is refused, and nothing of it is stored.
    let other = if kind == OperationKind::Check {
        OperationKind::Prune
    } else {
        OperationKind::Check
    };
    let mut probe = agent.recovery.queue(other, None, Some(&repository));
    probe.start(at(11)).expect("a queued operation starts");
    let decided = agent
        .store
        .try_start(&probe)
        .await
        .expect("the log answers a start");
    assert_eq!(
        decided,
        Start::Refused { running: op.id() },
        "the running {kind:?} holds the repository before the daemon starts again"
    );
}

#[given(
    regex = r#"^the log shows a backup of the plan "([^"]+)" that ended as (succeeded with warnings|succeeded|failed because the repository was unreachable|cancelled)$"#
)]
async fn backup_ended(agent: &mut Agent, plan: String, ending: String) {
    let mut op = agent
        .recovery
        .queue(OperationKind::Backup, Some(&plan), None);
    op.start(at(10)).expect("a queued operation starts");
    let ended = match ending.as_str() {
        "succeeded" => op.succeed(at(70)),
        "succeeded with warnings" => {
            op.record_warning(warning())
                .expect("a running operation records a warning");
            op.succeed(at(70))
        }
        "failed because the repository was unreachable" => {
            op.fail(unreachable_repository(), at(70))
        }
        "cancelled" => op.cancel(at(70)),
        other => panic!("the scenario ends an operation in a way that doesn't exist: {other}"),
    };
    ended.expect("a running operation ends");
    agent
        .store
        .save(&op, None)
        .await
        .expect("the log answers a save");
    let snapshot = detail(agent, op.id()).await;
    assert_eq!(snapshot.record.status, ending_status(&ending));
    agent
        .recovery
        .remember(&op, Some(&plan), None, Some(snapshot));
}

#[given("the log shows no operation at all")]
async fn empty_log(agent: &mut Agent) {
    let page = agent
        .store
        .list(&HistoryQuery::default())
        .await
        .expect("the log answers a list");
    assert!(
        page.operations.is_empty(),
        "the scenario's log starts empty"
    );
}

#[given(expr = "the log shows a {word} of the repository {string} as queued")]
async fn maintenance_queued(agent: &mut Agent, kind: String, repository: String) {
    let op = agent
        .recovery
        .queue(maintenance_kind(&kind), None, Some(&repository));
    agent
        .store
        .save(&op, None)
        .await
        .expect("the log answers a save");
    agent.recovery.remember(&op, None, Some(&repository), None);
}

#[given(expr = "the configuration no longer has the plan {string}")]
fn plan_removed(agent: &mut Agent, plan: String) {
    agent.recovery.removed.push(plan);
}

#[given(expr = "the plan {string} now backs up to the repository {string}")]
fn plan_moved(agent: &mut Agent, plan: String, repository: String) {
    agent.recovery.moved.push((plan, repository));
}

/// The agent's configuration can't be read, for the recovery and for the
/// notifications alike.
#[given("the configuration can't be read")]
fn config_unreadable(agent: &mut Agent) {
    agent.recovery.unreadable = true;
    agent.notifications.unreadable = true;
}

/// The start's recovery over `store`, announcing on a bus that tells the
/// scenario's notification rules of each piece of news as it is published.
async fn recovery(agent: &Agent, store: Arc<dyn OperationStore>) -> RecoverOperations {
    let notify = agent.notifications.notify_over(agent.store.clone()).await;
    RecoverOperations::new(store).with_events(Arc::new(Notifying(notify)))
}

#[when("the daemon starts again")]
async fn daemon_starts_again(agent: &mut Agent) {
    let recovery = recovery(agent, agent.store.clone()).await;
    let recovered = recovery.recover(agent.clock.now()).await;
    agent.recovery.starts.push(
        recovered
            .as_ref()
            .map(|recovered| recovered.ended.len())
            .map_err(Clone::clone),
    );
    let runs = Arc::new(RecordingRuns::new(agent.clock.clone()));
    if let Ok(recovered) = recovered {
        let world = &agent.recovery;
        let plans: Vec<(&str, &str)> = world
            .seeded
            .iter()
            .filter_map(|seeded| seeded.plan.as_deref())
            .filter(|plan| !world.removed.iter().any(|removed| removed == plan))
            .map(|plan| {
                let on = world
                    .moved
                    .iter()
                    .find(|(moved, _)| moved == plan)
                    .map_or("nas", |(_, repository)| repository.as_str());
                (plan, on)
            })
            .collect();
        let config = config_with_plans(&plans, repository_id);
        recovery
            .resume(
                recovered.queued,
                (!world.unreadable).then_some(&config),
                runs.as_ref(),
                agent.clock.now(),
            )
            .await;
    }
    agent.recovery.runs = Some(runs);
}

#[when("the daemon starts again on a log that cannot be written")]
async fn daemon_starts_on_unwritable_log(agent: &mut Agent) {
    let recovery = recovery(agent, Arc::new(UnwritableLog(agent.store.clone()))).await;
    let recovered = recovery.recover(agent.clock.now()).await;
    agent
        .recovery
        .starts
        .push(recovered.map(|recovered| recovered.ended.len()));
    // A start whose recovery failed refuses to serve: nothing runs again.
    agent.recovery.runs = Some(Arc::new(RecordingRuns::new(agent.clock.clone())));
}

/// The daemon's event bus, as far as the notification rules hear it: what is
/// published is told to them at once, and nothing else listens.
struct Notifying(Notify);

#[async_trait]
impl EventBus for Notifying {
    async fn publish(&self, event: UiEvent) {
        self.0.tell(event).await;
    }

    async fn subscribe(&self) -> BoxStream<'static, UiEvent> {
        Box::pin(futures::stream::empty())
    }
}

/// What the agent says when the queue ends an operation, as the scenario
/// words it.
fn queued_end(words: &str) -> QueuedEnd {
    match words {
        "its plan is gone" => QueuedEnd::PlanGone,
        "its plan's repository changed" => QueuedEnd::RepositoryChanged,
        "the configuration can't be read" => QueuedEnd::ConfigUnreadable,
        "the log doesn't keep its job" => QueuedEnd::JobNotKept,
        other => panic!("no step says why a queued operation ended as {other:?}"),
    }
}

fn said_ended(agent: &Agent, id: OperationId, kind: OperationKind, why: &str) {
    let said = agent
        .recovery
        .runs
        .as_ref()
        .expect("the daemon started again in the scenario")
        .reports();
    let ended = ScheduleReport::QueuedEnded {
        operation: id,
        kind,
        why: queued_end(why),
    };
    assert!(said.contains(&ended), "the agent said {said:?}");
}

#[then(regex = r#"^the agent said the queued backup of the plan "([^"]*)" ended because (.+)$"#)]
fn said_backup_ended(agent: &mut Agent, plan: String, why: String) {
    let id = agent.recovery.backup(&plan).id;
    said_ended(agent, id, OperationKind::Backup, &why);
}

#[then(
    regex = r#"^the agent said the queued (\w+) of the repository "([^"]*)" ended because (.+)$"#
)]
fn said_maintenance_ended(agent: &mut Agent, kind: String, repository: String, why: String) {
    let kind = maintenance_kind(&kind);
    let id = agent.recovery.maintenance(kind, &repository).id;
    said_ended(agent, id, kind, &why);
}

fn ran_again(agent: &Agent) -> Vec<Operation> {
    agent
        .recovery
        .runs
        .as_ref()
        .expect("the daemon started again in the scenario")
        .resumed()
}

#[then(expr = "the backup of the plan {string} runs again under its own ID")]
async fn backup_runs_again(agent: &mut Agent, plan: String) {
    let id = agent.recovery.backup(&plan).id;
    let queued = detail(agent, id).await.record;
    assert_eq!(
        queued.status,
        OperationStatus::Queued,
        "still queued in the log"
    );
    let again: Vec<Operation> = ran_again(agent)
        .into_iter()
        .filter(|operation| operation.id() == id)
        .collect();
    match again.as_slice() {
        [operation] => {
            assert_eq!(operation.status(), &OperationStatus::Queued);
            assert_eq!(operation.kind(), OperationKind::Backup);
            assert_eq!(operation.trigger(), queued.trigger);
            assert_eq!(operation.target(), &queued.target);
        }
        runs => panic!("the backup ran again {} times", runs.len()),
    }
}

#[then(expr = "the queued backups run again in the order {string}, {string}")]
fn run_again_in_order(agent: &mut Agent, first: String, second: String) {
    let order: Vec<OperationId> = [first, second]
        .iter()
        .map(|plan| agent.recovery.backup(plan).id)
        .collect();
    let ran: Vec<OperationId> = ran_again(agent).iter().map(Operation::id).collect();
    assert_eq!(ran, order);
}

#[then("nothing runs again")]
fn nothing_runs_again(agent: &mut Agent) {
    assert_eq!(ran_again(agent), Vec::new());
}

#[then(expr = "the log shows the backup of the plan {string} as failed because the daemon stopped")]
async fn backup_failed_daemon_stopped(agent: &mut Agent, plan: String) {
    let id = agent.recovery.backup(&plan).id;
    assert_failed_daemon_stopped(agent, id).await;
}

#[then(
    expr = "the log shows the {word} of the repository {string} as failed because the daemon stopped"
)]
async fn maintenance_failed_daemon_stopped(agent: &mut Agent, kind: String, repository: String) {
    let id = agent
        .recovery
        .maintenance(maintenance_kind(&kind), &repository)
        .id;
    assert_failed_daemon_stopped(agent, id).await;
}

async fn assert_failed_daemon_stopped(agent: &Agent, id: OperationId) {
    let status = detail(agent, id).await.record.status;
    assert_eq!(
        status,
        OperationStatus::Failed {
            reason: FailureReason::DaemonStopped
        }
    );
    match status {
        OperationStatus::Failed { reason } => assert_eq!(reason.code(), "daemon-stopped"),
        other => panic!("the operation is not failed: {other:?}"),
    }
}

#[then(expr = "the log never shows the backup of the plan {string} as succeeded")]
async fn backup_never_succeeded(agent: &mut Agent, plan: String) {
    let id = agent.recovery.backup(&plan).id;
    let status = detail(agent, id).await.record.status;
    assert!(
        !matches!(
            status,
            OperationStatus::Succeeded | OperationStatus::SucceededWithWarnings
        ),
        "the log shows the backup as {status:?}"
    );
}

#[then(expr = "the log still shows {int} warning(s) on the backup of the plan {string}")]
async fn backup_keeps_warnings(agent: &mut Agent, count: usize, plan: String) {
    let id = agent.recovery.backup(&plan).id;
    let held = detail(agent, id).await;
    assert_eq!(held.record.warning_count, count as u64);
    assert_eq!(held.warnings.len(), count);
}

#[then(expr = "the log still shows the backup of the plan {string} as running")]
async fn backup_still_running(agent: &mut Agent, plan: String) {
    let id = agent.recovery.backup(&plan).id;
    assert_eq!(
        detail(agent, id).await.record.status,
        OperationStatus::Running
    );
}

#[then(
    regex = r#"^the log still shows the backup of the plan "([^"]+)" as (succeeded with warnings|succeeded|failed because the repository was unreachable|cancelled)$"#
)]
async fn backup_keeps_its_ending(agent: &mut Agent, plan: String, ending: String) {
    let seeded = agent.recovery.backup(&plan);
    let id = seeded.id;
    let before = seeded
        .ended
        .clone()
        .expect("the scenario put an ended backup in the log");
    let now = detail(agent, id).await;
    assert_eq!(now.record.status, ending_status(&ending));
    assert_eq!(now, before, "nothing of an ended operation changed");
}

#[then(expr = "the repository {string} is free for a prune and for a check")]
async fn repository_is_free(agent: &mut Agent, repository: String) {
    for kind in [OperationKind::Prune, OperationKind::Check] {
        let mut op = agent.recovery.queue(kind, None, Some(&repository));
        start(agent, &mut op).await;
        // Ending it frees the repository again for the next kind.
        op.succeed(at(80)).expect("a running operation ends");
        agent
            .store
            .save(&op, None)
            .await
            .expect("the log answers a save");
    }
}

#[then(expr = "the daemon's start ended {int} operation(s)")]
fn start_ended(agent: &mut Agent, count: usize) {
    assert_eq!(agent.recovery.last_start(), &Ok(count));
}

#[then(expr = "the daemon's first start ended {int} operation(s)")]
fn first_start_ended(agent: &mut Agent, count: usize) {
    let first = agent
        .recovery
        .starts
        .first()
        .expect("the daemon started again in the scenario");
    assert_eq!(first, &Ok(count));
}

#[then("the daemon's start reports that the log could not be written")]
fn start_reports_unwritable(agent: &mut Agent) {
    assert_matches!(
        agent.recovery.last_start(),
        Err(StoreError::Unavailable { .. }),
        "the start reported {:?}",
        agent.recovery.last_start()
    );
}

/// The scenario's log, but one that refuses to end operations.
struct UnwritableLog(Arc<InMemoryOperationStore>);

#[async_trait]
impl OperationStore for UnwritableLog {
    async fn save(
        &self,
        operation: &Operation,
        result: Option<&EngineResult>,
    ) -> Result<(), StoreError> {
        self.0.save(operation, result).await
    }

    async fn try_start(&self, operation: &Operation) -> Result<Start, StoreError> {
        self.0.try_start(operation).await
    }

    async fn get(&self, id: OperationId) -> Result<Option<OperationDetail>, StoreError> {
        self.0.get(id).await
    }

    async fn list(&self, query: &HistoryQuery) -> Result<HistoryPage, StoreError> {
        self.0.list(query).await
    }

    async fn ever_succeeded(&self, plan: &domain::PlanId) -> Result<bool, StoreError> {
        self.0.ever_succeeded(plan).await
    }

    async fn recover(&self, _reason: &FailureReason) -> Result<app::ports::Recovered, StoreError> {
        Err(StoreError::Unavailable {
            detail: "the log cannot be written".to_owned(),
        })
    }
}
