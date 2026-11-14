//! Startup recovery: ending what an earlier run left running, and running
//! again what it left queued (technical design §5, §9; decisions 40 and 73).

use std::sync::Arc;

use domain::Timestamp;
use domain::execution::{FailureReason, OperationKind, Target};
use domain::planning::AgentConfig;

use crate::ports::{EventBus, OperationRecord, OperationStore, Recovered, StoreError};
use crate::schedule::{PlanRuns, ScheduleReport};

/// Why the persistent queue can't run a queued operation again at the start,
/// so that it ends as failed with [`FailureReason::DaemonStopped`]
/// (decision 73).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QueuedEnd {
    /// The configuration couldn't be read, so no backup's job can be built.
    ConfigUnreadable,
    /// The backup's plan is no longer configured.
    PlanGone,
    /// The backup's plan now backs up to another repository than the one the
    /// operation was queued for.
    RepositoryChanged,
    /// Not a backup: the log doesn't keep the job of any other kind.
    JobNotKept,
}

/// Ends every operation the log still shows as running, as failed with
/// [`FailureReason::DaemonStopped`], and hands back the queued ones. The
/// daemon runs it once at start, before it accepts a request: nothing is
/// running yet, so whatever the log calls running was left by a process
/// that stopped, and a running maintenance operation among it must stop
/// holding its repository (I7).
///
/// It ends running operations as failed, never as succeeded and never by
/// running them again: what an operation did before the process stopped is
/// unknown. It runs no hooks, because they belonged to the process that is
/// gone, and announces each operation it ends as any failed operation is
/// announced (§4.4, §11), once the log has its end: a start that couldn't
/// write the end announces nothing, and the next start, which ends it,
/// announces it then. A queued operation never reached the engine, so it is
/// the persistent queue's (§5): [`resume`](Self::resume) runs it again.
pub struct RecoverOperations {
    store: Arc<dyn OperationStore>,
    events: Option<Arc<dyn EventBus>>,
}

impl RecoverOperations {
    pub fn new(store: Arc<dyn OperationStore>) -> Self {
        Self {
            store,
            events: None,
        }
    }

    /// Announces on `events` each operation it ends as failed: the
    /// notification rules hear it there (§11).
    #[must_use]
    pub fn with_events(mut self, events: Arc<dyn EventBus>) -> Self {
        self.events = Some(events);
        self
    }

    /// Ends the running operations, announces each as failed at `now`, and
    /// returns them and the queued ones, oldest first. An error means the
    /// log could not be changed and nothing was, so nothing is announced:
    /// the daemon refuses to start rather than serve with a repository held
    /// for good.
    pub async fn recover(&self, now: Timestamp) -> Result<Recovered, StoreError> {
        let _ = (now, &self.events);
        self.store.recover(&FailureReason::DaemonStopped).await
    }

    /// Runs again, oldest first, each queued backup of a plan `config` still
    /// has, on the repository it was queued for, under its own ID and
    /// trigger (decision 73). `config` is `None` when the configuration
    /// can't be read. A queued operation the queue can't run again ends as
    /// failed with [`FailureReason::DaemonStopped`] at `now`, as decision 40
    /// ended every queued one, and `runs` is told why ([`QueuedEnd`]). A
    /// write the log refuses is reported through `runs` and leaves that
    /// operation queued.
    pub async fn resume(
        &self,
        queued: Vec<OperationRecord>,
        config: Option<&AgentConfig>,
        runs: &dyn PlanRuns,
        now: Timestamp,
    ) {
        for record in queued {
            // Recovery hands back queued rows only; one that isn't is the
            // log's fault, said and left as it is rather than run or ended.
            let Some(mut operation) = record.queued_operation() else {
                runs.report(ScheduleReport::Store(StoreError::Corrupt {
                    detail: format!("operation {:?} came back as queued and isn't", record.id),
                }));
                continue;
            };
            let why = match (record.kind, config) {
                (OperationKind::Backup, Some(config)) => match ended_why(&record.target, config) {
                    None => {
                        runs.resume(operation).await;
                        continue;
                    }
                    Some(why) => why,
                },
                (OperationKind::Backup, None) => QueuedEnd::ConfigUnreadable,
                _ => QueuedEnd::JobNotKept,
            };
            runs.report(ScheduleReport::QueuedEnded {
                operation: record.id,
                kind: record.kind,
                why,
            });
            // A queued operation never ends otherwise: fail is allowed.
            if operation.fail(FailureReason::DaemonStopped, now).is_ok()
                && let Err(error) = self.store.save(&operation, None).await
            {
                runs.report(ScheduleReport::Store(error));
            }
        }
    }
}

/// Why a queued backup on `target` can't run again under `config`, or
/// `None` when it can: its plan is configured, and backs up to the
/// repository the backup was queued for.
fn ended_why(target: &Target, config: &AgentConfig) -> Option<QueuedEnd> {
    let Some(plan) = target.plan.as_ref().and_then(|plan| config.plan(plan)) else {
        return Some(QueuedEnd::PlanGone);
    };
    let repository = config
        .repository(&plan.repository)
        .and_then(|repository| repository.id.as_ref());
    (!target.is_on(repository)).then_some(QueuedEnd::RepositoryChanged)
}

#[cfg(test)]
#[cfg(feature = "fakes")]
mod tests {
    use std::assert_matches;
    use std::sync::{Arc, Mutex, PoisonError};

    use api::{ErrorCode, UiEvent};
    use async_trait::async_trait;
    use domain::execution::{
        FailureReason, Operation, OperationKind, OperationStatus, Target, Trigger,
    };
    use domain::planning::AgentConfig;
    use domain::{OperationId, PlanId, Timestamp, Uuid};
    use futures::stream::BoxStream;

    use super::{QueuedEnd, RecoverOperations};
    use crate::fakes::InMemoryOperationStore;
    use crate::ports::{
        EngineResult, EventBus, HistoryPage, HistoryQuery, OperationDetail, OperationRecord,
        OperationStore, Recovered, Start, StoreError,
    };
    use crate::schedule::{PlanRuns, ScheduleReport};

    /// A bus that keeps what is published on it.
    #[derive(Default)]
    struct Published(Mutex<Vec<UiEvent>>);

    impl Published {
        fn news(&self) -> Vec<UiEvent> {
            self.0
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone()
        }
    }

    #[async_trait]
    impl EventBus for Published {
        async fn publish(&self, event: UiEvent) {
            self.0
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(event);
        }

        async fn subscribe(&self) -> BoxStream<'static, UiEvent> {
            Box::pin(futures::stream::empty())
        }
    }

    /// The news that `id` failed because the daemon stopped, told at `at`.
    fn daemon_stopped(id: OperationId, at: Timestamp) -> UiEvent {
        UiEvent::OperationFailed {
            operation: id.as_uuid().hyphenated().to_string(),
            code: ErrorCode::DaemonStopped,
            args: Vec::new(),
            finished_at: at.to_string(),
        }
    }

    /// The in-memory log, but one that refuses every save.
    struct RefusesSaves(Arc<InMemoryOperationStore>);

    #[async_trait]
    impl OperationStore for RefusesSaves {
        async fn save(&self, _: &Operation, _: Option<&EngineResult>) -> Result<(), StoreError> {
            Err(StoreError::Unavailable {
                detail: "disk I/O error".to_owned(),
            })
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

        async fn ever_succeeded(&self, plan: &PlanId) -> Result<bool, StoreError> {
            self.0.ever_succeeded(plan).await
        }

        async fn recover(&self, reason: &FailureReason) -> Result<Recovered, StoreError> {
            self.0.recover(reason).await
        }
    }

    #[derive(Default)]
    struct Recording {
        resumed: Mutex<Vec<Operation>>,
        reports: Mutex<Vec<ScheduleReport>>,
    }

    #[async_trait]
    impl PlanRuns for Recording {
        async fn start(&self, _: &PlanId, _: Trigger, _: crate::queue::Place) {}

        async fn resume(&self, operation: Operation) {
            self.resumed
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(operation);
        }

        fn report(&self, report: ScheduleReport) {
            self.reports
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(report);
        }
    }

    fn at(second: i64) -> Timestamp {
        Timestamp::from_second(1_800_000_000 + second).unwrap()
    }

    #[tokio::test]
    async fn a_record_that_is_not_queued_is_reported_and_left_as_it_is() {
        let id = OperationId::from_uuid(Uuid::from_u128(0x274));
        let mut running = Operation::queue(
            id,
            OperationKind::Prune,
            Trigger::Manual,
            Target {
                plan: None,
                repository: None,
            },
            at(0),
        );
        running.start(at(1)).unwrap();
        let store = Arc::new(InMemoryOperationStore::new());
        store.save(&running, None).await.unwrap();
        let runs = Recording::default();

        RecoverOperations::new(store.clone())
            .resume(
                vec![OperationRecord::of(&running, None)],
                Some(&AgentConfig::default()),
                &runs,
                at(10),
            )
            .await;

        let reports = runs
            .reports
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        assert_matches!(
            reports.as_slice(),
            [ScheduleReport::Store(StoreError::Corrupt { .. })],
            "{reports:?}"
        );
        assert!(
            runs.resumed
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .is_empty()
        );
        let kept = store.get(id).await.unwrap().map(|detail| detail.record);
        assert_eq!(kept, Some(OperationRecord::of(&running, None)));
    }

    /// A queued operation of `kind` on `plan`, saved in `store`.
    async fn queued(
        store: &InMemoryOperationStore,
        n: u128,
        kind: OperationKind,
        plan: Option<&str>,
    ) -> Operation {
        let operation = Operation::queue(
            OperationId::from_uuid(Uuid::from_u128(0x2740 + n)),
            kind,
            Trigger::Schedule,
            Target {
                plan: plan.map(|plan| PlanId::parse(plan).unwrap()),
                repository: None,
            },
            at(0),
        );
        store.save(&operation, None).await.unwrap();
        operation
    }

    /// What `runs` was told of the queued operations that ended.
    fn ended(runs: &Recording) -> Vec<ScheduleReport> {
        runs.reports
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .filter(|report| matches!(report, ScheduleReport::QueuedEnded { .. }))
            .cloned()
            .collect()
    }

    async fn is_failed(store: &InMemoryOperationStore, operation: &Operation) -> bool {
        let status = store
            .get(operation.id())
            .await
            .unwrap()
            .unwrap()
            .record
            .status;
        status
            == OperationStatus::Failed {
                reason: FailureReason::DaemonStopped,
            }
    }

    #[tokio::test]
    async fn every_queued_operation_that_ends_says_why() {
        let store = Arc::new(InMemoryOperationStore::new());
        let backup = queued(&store, 1, OperationKind::Backup, Some("home")).await;
        let check = queued(&store, 2, OperationKind::Check, None).await;
        let runs = Recording::default();

        RecoverOperations::new(store.clone())
            .resume(
                vec![
                    OperationRecord::of(&backup, None),
                    OperationRecord::of(&check, None),
                ],
                Some(&AgentConfig::default()),
                &runs,
                at(10),
            )
            .await;

        assert_eq!(
            ended(&runs),
            vec![
                ScheduleReport::QueuedEnded {
                    operation: backup.id(),
                    kind: OperationKind::Backup,
                    why: QueuedEnd::PlanGone,
                },
                ScheduleReport::QueuedEnded {
                    operation: check.id(),
                    kind: OperationKind::Check,
                    why: QueuedEnd::JobNotKept,
                },
            ]
        );
        assert!(is_failed(&store, &backup).await);
        assert!(is_failed(&store, &check).await);
    }

    #[tokio::test]
    async fn a_configuration_that_cannot_be_read_ends_every_queued_operation() {
        let store = Arc::new(InMemoryOperationStore::new());
        let backup = queued(&store, 3, OperationKind::Backup, Some("home")).await;
        let prune = queued(&store, 4, OperationKind::Prune, None).await;
        let runs = Recording::default();

        RecoverOperations::new(store.clone())
            .resume(
                vec![
                    OperationRecord::of(&backup, None),
                    OperationRecord::of(&prune, None),
                ],
                None,
                &runs,
                at(10),
            )
            .await;

        assert_eq!(
            ended(&runs),
            vec![
                ScheduleReport::QueuedEnded {
                    operation: backup.id(),
                    kind: OperationKind::Backup,
                    why: QueuedEnd::ConfigUnreadable,
                },
                ScheduleReport::QueuedEnded {
                    operation: prune.id(),
                    kind: OperationKind::Prune,
                    why: QueuedEnd::JobNotKept,
                },
            ]
        );
        assert!(
            runs.resumed
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .is_empty()
        );
        assert!(is_failed(&store, &backup).await);
        assert!(is_failed(&store, &prune).await);
    }

    /// An operation of `kind` on `plan`, started and saved in `store`.
    async fn running(
        store: &InMemoryOperationStore,
        n: u128,
        kind: OperationKind,
        plan: Option<&str>,
    ) -> Operation {
        let mut operation = Operation::queue(
            OperationId::from_uuid(Uuid::from_u128(0x2750 + n)),
            kind,
            Trigger::Schedule,
            Target {
                plan: plan.map(|plan| PlanId::parse(plan).unwrap()),
                repository: None,
            },
            at(0),
        );
        operation.start(at(1)).unwrap();
        store.save(&operation, None).await.unwrap();
        operation
    }

    #[tokio::test]
    async fn each_running_operation_a_start_ends_is_announced_as_failed_once() {
        let store = Arc::new(InMemoryOperationStore::new());
        let backup = running(&store, 1, OperationKind::Backup, Some("home")).await;
        let check = running(&store, 2, OperationKind::Check, None).await;
        queued(&store, 3, OperationKind::Backup, Some("docs")).await;
        let mut ended = running(&store, 4, OperationKind::Backup, Some("photos")).await;
        ended.succeed(at(5)).unwrap();
        store.save(&ended, None).await.unwrap();
        let bus = Arc::new(Published::default());
        let recovery = RecoverOperations::new(store.clone()).with_events(bus.clone());

        recovery.recover(at(20)).await.unwrap();
        recovery.recover(at(30)).await.unwrap();

        assert_eq!(
            bus.news(),
            vec![
                daemon_stopped(backup.id(), at(20)),
                daemon_stopped(check.id(), at(20)),
            ],
            "each operation the first start ended, by ID, and nothing the second start found"
        );
    }

    #[tokio::test]
    async fn a_queued_operation_resume_ends_is_announced_once_the_log_has_its_end() {
        let store = Arc::new(InMemoryOperationStore::new());
        let backup = queued(&store, 5, OperationKind::Backup, Some("home")).await;
        let bus = Arc::new(Published::default());

        RecoverOperations::new(store.clone())
            .with_events(bus.clone())
            .resume(
                vec![OperationRecord::of(&backup, None)],
                Some(&AgentConfig::default()),
                &Recording::default(),
                at(10),
            )
            .await;

        assert!(is_failed(&store, &backup).await);
        assert_eq!(bus.news(), vec![daemon_stopped(backup.id(), at(10))]);
    }

    #[tokio::test]
    async fn a_queued_end_is_announced_only_by_the_start_whose_write_the_log_took() {
        let log = Arc::new(InMemoryOperationStore::new());
        let backup = queued(&log, 6, OperationKind::Backup, Some("home")).await;
        let bus = Arc::new(Published::default());
        let runs = Recording::default();
        let resume = |store: Arc<dyn OperationStore>, now| {
            let queued = vec![OperationRecord::of(&backup, None)];
            let recovery = RecoverOperations::new(store).with_events(bus.clone());
            let runs = &runs;
            async move {
                recovery
                    .resume(queued, Some(&AgentConfig::default()), runs, now)
                    .await;
            }
        };

        resume(Arc::new(RefusesSaves(log.clone())), at(10)).await;
        assert_eq!(bus.news(), Vec::new(), "the log still shows it queued");
        assert_matches!(
            runs.reports
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .as_slice(),
            [
                ScheduleReport::QueuedEnded { .. },
                ScheduleReport::Store(StoreError::Unavailable { .. })
            ]
        );

        resume(log.clone(), at(20)).await;
        assert!(is_failed(&log, &backup).await);
        assert_eq!(bus.news(), vec![daemon_stopped(backup.id(), at(20))]);
    }
}
