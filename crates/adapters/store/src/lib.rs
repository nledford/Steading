//! Store adapter: the SQLite database behind the operation log and history
//! (technical design §8.3 and §9). Queries are checked at run time by the
//! contract suite, not at compile time (§8.2).

#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unimplemented,
    clippy::todo,
    clippy::unreachable
)]

mod codec;
mod error_args;
mod locations;
mod report;
mod row;
mod sql;
mod summary;

#[cfg(test)]
mod pool_tests;

use std::fmt::Display;
use std::path::Path;
use std::time::Duration;

use app::ports::{
    EngineResult, HistoryPage, HistoryQuery, OperationDetail, OperationStore, PendingRetry,
    PlannedRun, Recovered, ScheduleStore, Start, StoreError, maintained_repository,
};
use async_trait::async_trait;
use domain::execution::locations::LocationsRecord;
use domain::execution::retry::RetryScheduled;
use domain::execution::{
    FailureReason, MAX_STORED_WARNINGS, Operation, OperationStatus, warning_kinds,
};
use domain::{OperationId, PlanId, RepositoryAlias, RepositoryId, Timestamp};
use sqlx::migrate::{MigrateError, Migrator};
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use sqlx::{Connection, SqliteConnection, SqlitePool};
use tokio::sync::{RwLock, RwLockReadGuard};

use crate::report::cap;
use crate::summary::Summary;

/// The migrations this build ships, embedded when the crate is compiled (§8.3).
static MIGRATOR: Migrator = sqlx::migrate!("../../../migrations");

/// How long a statement waits for another connection's write lock.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// How long a call waits for a free connection.
const ACQUIRE_TIMEOUT: Duration = Duration::from_secs(10);

/// Connections in the pool: enough for readers beside a writer, few enough
/// that a burst of calls queues instead of piling locks on the file.
const MAX_CONNECTIONS: u32 = 4;

/// The `agent_id` of every operation this agent ran (§9: agents the hub knows
/// have registry IDs).
const LOCAL_AGENT: &str = "local";

/// Why the database couldn't be opened.
#[derive(Debug)]
pub enum OpenError {
    /// The file can't be created or opened as a database. `detail` is SQLite's own message.
    Database { detail: String },
    /// The database was written by a newer version: it holds a migration this
    /// build doesn't know. Opening it would risk its data.
    NewerSchema { version: i64 },
    /// A migration this build ships failed or doesn't match the one applied.
    Migration { detail: String },
}

impl std::fmt::Display for OpenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(self, f)
    }
}

impl std::error::Error for OpenError {}

/// The operation log in one SQLite file, in WAL mode.
///
/// A few connections serve concurrent calls, and readers never wait for a
/// writer. A write takes the database's write lock first and waits up to five
/// seconds for it; a call that can't get a connection or the lock in time is
/// [`StoreError::Unavailable`], never a hang.
#[derive(Debug)]
pub struct SqliteStore {
    pool: SqlitePool,
    /// Held shared by each call for as long as it uses the database, and
    /// alone by [`SqliteStore::close`], which so waits for the calls in flight.
    calls: RwLock<()>,
}

impl SqliteStore {
    /// Opens the database at `path`, creating it when missing, and brings its
    /// schema up to date.
    ///
    /// The directory must exist: the data directory is the caller's to create.
    /// A database that holds a migration this build doesn't ship, because a
    /// newer version wrote it, is refused with [`OpenError::NewerSchema`] and
    /// left as it was. A failed open leaves no connection behind.
    pub async fn open(path: &Path) -> Result<Self, OpenError> {
        let options = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Normal)
            .busy_timeout(BUSY_TIMEOUT)
            .foreign_keys(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(MAX_CONNECTIONS)
            .acquire_timeout(ACQUIRE_TIMEOUT)
            .connect_with(options)
            .await
            .map_err(|error| OpenError::Database {
                detail: cap(error.to_string()),
            })?;
        if let Err(error) = MIGRATOR.run(&pool).await {
            pool.close().await;
            return Err(match error {
                MigrateError::VersionMissing(version) => OpenError::NewerSchema { version },
                other => OpenError::Migration {
                    detail: cap(other.to_string()),
                },
            });
        }
        Ok(Self {
            pool,
            calls: RwLock::new(()),
        })
    }

    /// Closes the pool once the calls in flight, reads and writes, have
    /// ended: a write that was in flight has committed or failed by the time
    /// it returns, and no connection to the file is left open, short of one
    /// sqlx gave up on closing: a close that took over five seconds, or a
    /// connection it found broken. A call made after it, or while it waits,
    /// is [`StoreError::Unavailable`].
    ///
    /// sqlx hands a connection back to the pool in a task of its own once
    /// its call has ended, and its `Pool::close` waits for those tasks too,
    /// so none outlives `close` (#174). The store still waits for its calls
    /// itself rather than leave that to the pool: a call made before `close`
    /// that is still waiting for a free connection ends as it would have,
    /// where the pool alone would refuse it once closing began.
    pub async fn close(&self) {
        let _alone = self.calls.write().await;
        self.pool.close().await;
    }

    /// The store, held for one call until it has done with the database.
    /// Once `close` waits, a new call waits behind it and then finds the
    /// pool closed: the lock is fair, so calls can't keep `close` waiting.
    async fn call(&self) -> RwLockReadGuard<'_, ()> {
        self.calls.read().await
    }
}

/// A database or connection failure.
fn unavailable(error: sqlx::Error) -> StoreError {
    StoreError::Unavailable {
        detail: cap(error.to_string()),
    }
}

/// An operation that can't be put in its columns: not expected of any
/// operation, but reported rather than assumed.
fn encoding(error: impl Display) -> StoreError {
    StoreError::Unavailable {
        detail: cap(format!("the operation can't be encoded: {error}")),
    }
}

/// An operation as its columns hold it, worked out before the write lock is
/// taken: whatever can fail to encode fails before the database is touched.
struct Encoded<'a> {
    id: String,
    failure: Option<codec::FailureColumns<'a>>,
    summary: Option<String>,
    snapshot: Option<&'a str>,
    locations: Option<String>,
}

impl<'a> Encoded<'a> {
    fn new(operation: &'a Operation, result: Option<&'a EngineResult>) -> Result<Self, StoreError> {
        let failure = match operation.status() {
            OperationStatus::Failed { reason } => {
                Some(codec::encode_failure(reason).map_err(encoding)?)
            }
            _ => None,
        };
        let summary = result
            .map(|result| serde_json::to_string(&Summary::from(result)))
            .transpose()
            .map_err(encoding)?;
        let snapshot = match result {
            Some(EngineResult::Backup(backup)) => Some(backup.snapshot.as_str()),
            _ => None,
        };
        let locations = operation
            .locations()
            .map(locations::encode)
            .transpose()
            .map_err(encoding)?;
        Ok(Self {
            id: operation.id().as_uuid().to_string(),
            failure,
            summary,
            snapshot,
            locations,
        })
    }
}

/// Writes the operation, replacing what an earlier save left of it, and its
/// warnings, on a connection that is inside a write transaction.
async fn write(
    conn: &mut SqliteConnection,
    operation: &Operation,
    encoded: &Encoded<'_>,
) -> Result<(), StoreError> {
    let id = encoded.id.as_str();
    let target = operation.target();
    sqlx::query(sql::UPSERT_OPERATION)
        .bind(id)
        .bind(LOCAL_AGENT)
        .bind(codec::kind_text(operation.kind()))
        .bind(target.plan.as_ref().map(PlanId::as_str))
        .bind(target.repository.as_ref().map(RepositoryId::as_str))
        .bind(codec::trigger_text(operation.trigger()))
        .bind(codec::status_text(operation.status()))
        .bind(operation.queued_at().as_millisecond())
        .bind(operation.started_at().map(Timestamp::as_millisecond))
        .bind(operation.finished_at().map(Timestamp::as_millisecond))
        .bind(encoded.snapshot)
        .bind(encoded.summary.as_deref())
        .bind(encoded.failure.as_ref().map(|failure| failure.code))
        .bind(
            encoded
                .failure
                .as_ref()
                .map(|failure| failure.args.as_str()),
        )
        .bind(encoded.failure.as_ref().and_then(|failure| failure.detail))
        .bind(i64::try_from(operation.warning_count()).unwrap_or(i64::MAX))
        .bind(operation.repository_alias().map(RepositoryAlias::as_str))
        .bind(
            operation
                .locations()
                .and_then(LocationsRecord::used)
                .map(i64::from),
        )
        .bind(encoded.locations.as_deref())
        .execute(&mut *conn)
        .await
        .map_err(unavailable)?;
    sqlx::query(sql::DELETE_WARNINGS)
        .bind(id)
        .execute(&mut *conn)
        .await
        .map_err(unavailable)?;
    for warning in operation.warnings().iter().take(MAX_STORED_WARNINGS) {
        sqlx::query(sql::INSERT_WARNING)
            .bind(id)
            .bind(warning.item.as_deref())
            .bind(warning.during.as_str())
            .bind(codec::warning_code_text(warning.kind))
            .bind(warning.detail.as_str())
            .execute(&mut *conn)
            .await
            .map_err(unavailable)?;
    }
    Ok(())
}

/// One save, as one transaction. The write lock is taken up front, so the
/// busy timeout covers the wait for it, and the whole save is one
/// transaction: it all happens or none of it does. The one-maintenance index
/// is the backstop for I7: a save that would make a second running
/// maintenance operation fails here and stores nothing.
///
/// A write is cut short when its future is dropped before it ends: a run
/// does that to the write that says an operation is running when a cancel or
/// the deadline comes first (§9). sqlx rolls back what such a write began, so
/// its connection goes back to the pool outside any transaction: the
/// connection's own thread rolls back a `BEGIN` whose answer nobody took, and
/// a dropped `Transaction` queues its rollback, which runs before the pool
/// takes the connection back. sqlx 0.8 left a write dropped between those two
/// points inside its transaction, holding the database's write lock, and
/// every write after it failed (#123); `tests/cancellation.rs` cuts a save
/// and a start at each of their await points.
async fn save_in_transaction(
    conn: &mut SqliteConnection,
    operation: &Operation,
    encoded: &Encoded<'_>,
) -> Result<(), StoreError> {
    let mut tx = conn
        .begin_with("BEGIN IMMEDIATE")
        .await
        .map_err(unavailable)?;
    write(&mut tx, operation, encoded).await?;
    tx.commit().await.map_err(unavailable)
}

/// One start, as one transaction: the lookup of the repository's holder and
/// the write. The lock is taken before the lookup, so no other handle can
/// start a maintenance operation between the lookup and the write.
async fn start_in_transaction(
    conn: &mut SqliteConnection,
    operation: &Operation,
    encoded: &Encoded<'_>,
) -> Result<Start, StoreError> {
    let mut tx = conn
        .begin_with("BEGIN IMMEDIATE")
        .await
        .map_err(unavailable)?;
    if let Some(repository) =
        maintained_repository(operation.kind(), operation.status(), operation.target())
    {
        let holder = sqlx::query(sql::FIND_RUNNING_MAINTENANCE)
            .bind(repository.as_str())
            .bind(encoded.id.as_str())
            .fetch_optional(&mut *tx)
            .await
            .map_err(unavailable)?;
        if let Some(holder) = holder {
            // A holder that can't be read is an error, never a way in: a
            // return here drops the transaction, which rolls it back.
            let running = row::operation_id(&holder)?;
            tx.rollback().await.map_err(unavailable)?;
            return Ok(Start::Refused { running });
        }
    }
    write(&mut tx, operation, encoded).await?;
    tx.commit().await.map_err(unavailable)?;
    Ok(Start::Started)
}

#[async_trait]
impl OperationStore for SqliteStore {
    async fn save(
        &self,
        operation: &Operation,
        result: Option<&EngineResult>,
    ) -> Result<(), StoreError> {
        let encoded = Encoded::new(operation, result)?;
        let _call = self.call().await;
        let mut conn = self.pool.acquire().await.map_err(unavailable)?;
        save_in_transaction(&mut conn, operation, &encoded).await
    }

    async fn try_start(&self, operation: &Operation) -> Result<Start, StoreError> {
        if !matches!(operation.status(), OperationStatus::Running) {
            return Err(StoreError::Unavailable {
                detail: cap(format!(
                    "only a running operation can be started, not a {} one",
                    codec::status_text(operation.status())
                )),
            });
        }
        let encoded = Encoded::new(operation, None)?;
        let _call = self.call().await;
        let mut conn = self.pool.acquire().await.map_err(unavailable)?;
        start_in_transaction(&mut conn, operation, &encoded).await
    }

    async fn get(&self, id: OperationId) -> Result<Option<OperationDetail>, StoreError> {
        let id = id.as_uuid().to_string();
        let _call = self.call().await;
        // One read transaction: the row and its warnings are one snapshot, so
        // a save that lands in between can't be seen half done.
        let mut tx = self.pool.begin().await.map_err(unavailable)?;
        let found = sqlx::query(sql::GET_OPERATION)
            .bind(id.as_str())
            .fetch_optional(&mut *tx)
            .await
            .map_err(unavailable)?;
        let warning_rows = match found {
            Some(_) => {
                let most = i64::try_from(MAX_STORED_WARNINGS).unwrap_or(i64::MAX);
                sqlx::query(sql::GET_WARNINGS)
                    .bind(id.as_str())
                    .bind(most)
                    .fetch_all(&mut *tx)
                    .await
                    .map_err(unavailable)?
            }
            None => Vec::new(),
        };
        tx.commit().await.map_err(unavailable)?;

        let Some(found) = found else {
            return Ok(None);
        };
        let mut record = row::record(&found)?;
        let warnings = warning_rows
            .iter()
            .enumerate()
            .map(|(position, warning)| row::warning(&id, position, warning))
            .collect::<Result<Vec<_>, _>>()?;
        record.warning_kinds = warning_kinds(&warnings);
        Ok(Some(OperationDetail { record, warnings }))
    }

    async fn list(&self, query: &HistoryQuery) -> Result<HistoryPage, StoreError> {
        let limit = query.limit.get();
        // One row more than the page holds, only to learn whether another
        // page follows; it is never read.
        let fetch = i64::try_from(limit.saturating_add(1)).unwrap_or(i64::MAX);
        let after = query.after.map(|after| after.as_uuid().to_string());
        let plan = query.plan.as_ref().map(PlanId::as_str);
        let _call = self.call().await;
        // One read transaction: the rows and their warning codes are one
        // snapshot, so a save that lands in between can't be seen half done.
        let mut tx = self.pool.begin().await.map_err(unavailable)?;
        let rows = match (plan, after.as_deref()) {
            (None, None) => {
                sqlx::query(sql::LIST_FIRST_PAGE)
                    .bind(fetch)
                    .fetch_all(&mut *tx)
                    .await
            }
            (None, Some(after)) => {
                sqlx::query(sql::LIST_AFTER)
                    .bind(after)
                    .bind(fetch)
                    .fetch_all(&mut *tx)
                    .await
            }
            (Some(plan), None) => {
                sqlx::query(sql::LIST_PLAN_FIRST_PAGE)
                    .bind(plan)
                    .bind(fetch)
                    .fetch_all(&mut *tx)
                    .await
            }
            (Some(plan), Some(after)) => {
                sqlx::query(sql::LIST_PLAN_AFTER)
                    .bind(plan)
                    .bind(after)
                    .bind(fetch)
                    .fetch_all(&mut *tx)
                    .await
            }
        }
        .map_err(unavailable)?;
        let more = rows.len() > limit;
        let mut operations = rows
            .iter()
            .take(limit)
            .map(row::record)
            .collect::<Result<Vec<_>, _>>()?;
        let ids: Vec<String> = operations
            .iter()
            .map(|record| record.id.as_uuid().to_string())
            .collect();
        let kind_rows = if ids.is_empty() {
            Vec::new()
        } else {
            sqlx::query(sql::LIST_WARNING_KINDS)
                .bind(serde_json::to_string(&ids).map_err(encoding)?)
                .fetch_all(&mut *tx)
                .await
                .map_err(unavailable)?
        };
        tx.commit().await.map_err(unavailable)?;

        // Each code is once per operation in the rows; an unknown code reads
        // as one kind, so two of them name it once.
        for kind_row in &kind_rows {
            let (operation, kind) = row::warning_kind(kind_row)?;
            let record = ids
                .iter()
                .position(|id| *id == operation)
                .and_then(|position| operations.get_mut(position));
            if let Some(record) = record
                && !record.warning_kinds.contains(&kind)
            {
                record.warning_kinds.push(kind);
            }
        }
        let next = if more {
            operations.last().map(|record| record.id)
        } else {
            None
        };
        Ok(HistoryPage { operations, next })
    }

    async fn ever_succeeded(&self, plan: &PlanId) -> Result<bool, StoreError> {
        let _call = self.call().await;
        sqlx::query_scalar::<_, bool>(sql::EVER_SUCCEEDED)
            .bind(plan.as_str())
            .fetch_one(&self.pool)
            .await
            .map_err(unavailable)
    }

    async fn recover(&self, reason: &FailureReason) -> Result<Recovered, StoreError> {
        let failure = codec::encode_failure(reason).map_err(encoding)?;
        let _call = self.call().await;
        let mut conn = self.pool.acquire().await.map_err(unavailable)?;
        // One transaction under the write lock: every running row ends or
        // none does, and no save lands between the ends and the read of the
        // queue. Dropped before the commit, it rolls back.
        let mut tx = conn
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(unavailable)?;
        let ended = sqlx::query(sql::FAIL_RUNNING)
            .bind(codec::FAILED)
            .bind(failure.code)
            .bind(failure.args.as_str())
            .bind(failure.detail)
            .bind(codec::status_text(&OperationStatus::Running))
            .fetch_all(&mut *tx)
            .await
            .map_err(unavailable)?;
        let rows = sqlx::query(sql::LIST_QUEUED)
            .persistent(true)
            .fetch_all(&mut *tx)
            .await
            .map_err(unavailable)?;
        tx.commit().await.map_err(unavailable)?;
        // A row that can't be read is no reason to refuse the start: it is
        // reported, a running one ended all the same and a queued one left
        // queued as it is.
        let mut recovered = Recovered::default();
        for row in &ended {
            match row::operation_id(row) {
                Ok(id) => recovered.ended.push(id),
                Err(error) => recovered.unreadable.push(error),
            }
        }
        // RETURNING follows no order; IDs sort in time order (UUIDv7).
        recovered.ended.sort_unstable();
        for row in &rows {
            match row::record(row) {
                Ok(record) => recovered.queued.push(record),
                Err(error) => recovered.unreadable.push(error),
            }
        }
        Ok(recovered)
    }
}

#[async_trait]
impl ScheduleStore for SqliteStore {
    async fn planned_runs(&self) -> Result<Vec<PlannedRun>, StoreError> {
        let _call = self.call().await;
        let rows = sqlx::query(sql::PLANNED_RUNS)
            .fetch_all(&self.pool)
            .await
            .map_err(unavailable)?;
        rows.iter().map(row::planned_run).collect()
    }

    async fn plan_run(&self, run: &PlannedRun) -> Result<(), StoreError> {
        let _call = self.call().await;
        sqlx::query(sql::PLAN_RUN)
            .bind(run.plan.as_str())
            .bind(run.at.as_millisecond())
            .execute(&self.pool)
            .await
            .map_err(unavailable)?;
        Ok(())
    }

    async fn unplan(&self, plan: &PlanId) -> Result<(), StoreError> {
        let _call = self.call().await;
        sqlx::query(sql::UNPLAN)
            .bind(plan.as_str())
            .execute(&self.pool)
            .await
            .map_err(unavailable)?;
        Ok(())
    }

    async fn pending_retries(&self) -> Result<Vec<PendingRetry>, StoreError> {
        let _call = self.call().await;
        let rows = sqlx::query(sql::PENDING_RETRIES)
            .fetch_all(&self.pool)
            .await
            .map_err(unavailable)?;
        rows.iter().map(row::pending_retry).collect()
    }

    async fn keep_retry(&self, retry: &PendingRetry) -> Result<(), StoreError> {
        let _call = self.call().await;
        let RetryScheduled { attempt, at, until } = retry.retry;
        sqlx::query(sql::KEEP_RETRY)
            .bind(retry.plan.as_str())
            .bind(i64::from(attempt))
            .bind(at.as_millisecond())
            .bind(until.map(|until| until.as_millisecond()))
            .execute(&self.pool)
            .await
            .map_err(unavailable)?;
        Ok(())
    }

    async fn drop_retry(&self, plan: &PlanId) -> Result<(), StoreError> {
        let _call = self.call().await;
        sqlx::query(sql::DROP_RETRY)
            .bind(plan.as_str())
            .execute(&self.pool)
            .await
            .map_err(unavailable)?;
        Ok(())
    }
}
