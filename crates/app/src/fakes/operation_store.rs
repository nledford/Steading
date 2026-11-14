use std::collections::BTreeMap;
use std::sync::{Mutex, MutexGuard, PoisonError};

use async_trait::async_trait;
use domain::execution::{FailureReason, Operation, OperationStatus};
use domain::{OperationId, PlanId};

use crate::ports::{
    EngineResult, HistoryPage, HistoryQuery, OperationDetail, OperationRecord, OperationStore,
    Recovered, Start, StoreError,
};

/// An operation log that lives in memory, for use-case tests and acceptance
/// scenarios. It keeps what the real stores keep, and nothing else.
#[derive(Default)]
pub struct InMemoryOperationStore {
    stored: Mutex<BTreeMap<OperationId, OperationDetail>>,
}

impl InMemoryOperationStore {
    pub fn new() -> Self {
        Self::default()
    }

    // Each change is an insert or a plain assignment that cannot panic partway,
    // so a panic elsewhere can't leave the map half-written.
    fn stored(&self) -> MutexGuard<'_, BTreeMap<OperationId, OperationDetail>> {
        self.stored.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// The ID of a running maintenance operation, other than `record`, on the
/// repository `record` maintains: the one that would make `record` a second.
fn holder_other_than(
    stored: &BTreeMap<OperationId, OperationDetail>,
    record: &OperationRecord,
) -> Option<OperationId> {
    let repository = record.maintaining()?;
    stored
        .values()
        .map(|detail| &detail.record)
        .find(|other| other.id != record.id && other.maintaining() == Some(repository))
        .map(|other| other.id)
}

#[async_trait]
impl OperationStore for InMemoryOperationStore {
    async fn save(
        &self,
        operation: &Operation,
        result: Option<&EngineResult>,
    ) -> Result<(), StoreError> {
        let record = OperationRecord::of(operation, result);
        let mut stored = self.stored();
        // The backstop: a second running maintenance operation never gets in
        // by `save` either.
        if holder_other_than(&stored, &record).is_some() {
            return Err(StoreError::Unavailable {
                detail: "another maintenance operation is running on the repository".to_owned(),
            });
        }
        let detail = OperationDetail {
            record,
            warnings: operation.warnings().to_vec(),
        };
        stored.insert(operation.id(), detail);
        Ok(())
    }

    async fn try_start(&self, operation: &Operation) -> Result<Start, StoreError> {
        let record = OperationRecord::of(operation, None);
        if record.status != OperationStatus::Running {
            return Err(StoreError::Unavailable {
                detail: "only a running operation can be started".to_owned(),
            });
        }
        let mut stored = self.stored();
        if let Some(running) = holder_other_than(&stored, &record) {
            return Ok(Start::Refused { running });
        }
        let detail = OperationDetail {
            record,
            warnings: operation.warnings().to_vec(),
        };
        stored.insert(operation.id(), detail);
        Ok(Start::Started)
    }

    async fn get(&self, id: OperationId) -> Result<Option<OperationDetail>, StoreError> {
        Ok(self.stored().get(&id).cloned())
    }

    async fn list(&self, query: &HistoryQuery) -> Result<HistoryPage, StoreError> {
        let stored = self.stored();
        let mut matching = stored
            .iter()
            .rev()
            .filter(|(id, _)| query.after.is_none_or(|after| **id < after))
            .map(|(_, detail)| &detail.record)
            .filter(|record| {
                query
                    .plan
                    .as_ref()
                    .is_none_or(|plan| record.target.plan.as_ref() == Some(plan))
            });
        let operations: Vec<OperationRecord> =
            matching.by_ref().take(query.limit.get()).cloned().collect();
        let more_remain = matching.next().is_some();
        let next = operations
            .last()
            .filter(|_| more_remain)
            .map(|last| last.id);
        Ok(HistoryPage { operations, next })
    }

    async fn ever_succeeded(&self, plan: &PlanId) -> Result<bool, StoreError> {
        Ok(self.stored().values().any(|detail| {
            detail.record.target.plan.as_ref() == Some(plan)
                && matches!(
                    detail.record.status,
                    OperationStatus::Succeeded | OperationStatus::SucceededWithWarnings
                )
        }))
    }

    async fn recover(&self, reason: &FailureReason) -> Result<Recovered, StoreError> {
        let mut recovered = Recovered::default();
        // The map is in ID order, which is time order: the oldest first.
        for detail in self.stored().values_mut() {
            match detail.record.status {
                OperationStatus::Running => {
                    detail.record.status = OperationStatus::Failed {
                        reason: reason.clone(),
                    };
                    recovered.ended.push(detail.record.id);
                }
                OperationStatus::Queued => recovered.queued.push(detail.record.clone()),
                _ => {}
            }
        }
        Ok(recovered)
    }
}
