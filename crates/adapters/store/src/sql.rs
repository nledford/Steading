//! The store's SQL. Every statement is a whole static string with bound
//! parameters: the one choice made at run time is which string to run, never
//! a value formatted into one.

/// A `SELECT` of the columns `row::record` reads, from `operations`, followed
/// by `clause`s.
macro_rules! select_operations {
    ($($clause:literal),+ $(,)?) => {
        concat!(
            "SELECT id, kind, plan_id, repo_uuid, \"trigger\", status, queued_at, started_at, ",
            "finished_at, snapshot_id, summary_json, error_code, error_args, error_detail, ",
            "warning_count, repo_alias, location_no, locations_json FROM operations",
            $($clause),+
        )
    };
}

/// Writes an operation, or replaces the one with its ID: every column, so
/// nothing of an earlier save survives. Parameters, in order: `id`, `agent_id`,
/// `kind`, `plan_id`, `repo_uuid`, `trigger`, `status`, `queued_at`,
/// `started_at`, `finished_at`, `snapshot_id`, `summary_json`, `error_code`,
/// `error_args`, `error_detail`, `warning_count`, `repo_alias`, `location_no`,
/// `locations_json`.
pub(crate) const UPSERT_OPERATION: &str = concat!(
    "INSERT INTO operations (id, agent_id, kind, plan_id, repo_uuid, \"trigger\", status, ",
    "queued_at, started_at, finished_at, snapshot_id, summary_json, error_code, error_args, ",
    "error_detail, warning_count, repo_alias, location_no, locations_json) ",
    "VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) ",
    "ON CONFLICT(id) DO UPDATE SET ",
    "agent_id = excluded.agent_id, kind = excluded.kind, plan_id = excluded.plan_id, ",
    "repo_uuid = excluded.repo_uuid, \"trigger\" = excluded.\"trigger\", ",
    "status = excluded.status, queued_at = excluded.queued_at, ",
    "started_at = excluded.started_at, finished_at = excluded.finished_at, ",
    "snapshot_id = excluded.snapshot_id, summary_json = excluded.summary_json, ",
    "error_code = excluded.error_code, error_args = excluded.error_args, ",
    "error_detail = excluded.error_detail, warning_count = excluded.warning_count, ",
    "repo_alias = excluded.repo_alias, location_no = excluded.location_no, ",
    "locations_json = excluded.locations_json"
);

/// Parameter: the operation's ID.
pub(crate) const DELETE_WARNINGS: &str = "DELETE FROM op_warnings WHERE op_id = ?";

/// Parameters: the operation's ID, `item`, `during`, `code`, `detail`.
pub(crate) const INSERT_WARNING: &str =
    "INSERT INTO op_warnings (op_id, item, during, code, detail) VALUES (?, ?, ?, ?, ?)";

/// The ID of a running maintenance operation on a repository, other than the
/// operation being started. Parameters: the repository, the operation's own ID.
/// The literals are those of the `one_maintenance_per_repo` index, so it
/// serves the lookup.
pub(crate) const FIND_RUNNING_MAINTENANCE: &str = concat!(
    "SELECT id FROM operations WHERE repo_uuid = ? AND status = 'running' ",
    "AND kind IN ('forget', 'adopt', 'prune', 'check') AND id <> ? LIMIT 1"
);

/// Parameter: the operation's ID.
pub(crate) const GET_OPERATION: &str = select_operations!(" WHERE id = ?");

/// Parameters: the operation's ID, and the most warnings to read.
pub(crate) const GET_WARNINGS: &str =
    "SELECT item, during, code, detail FROM op_warnings WHERE op_id = ? ORDER BY rowid LIMIT ?";

/// The warning codes of a page of operations, each code of each operation
/// once, in the order the first warning of each was recorded (#508): the
/// kept warnings are the stored ones. Parameter: the operations' IDs as a
/// JSON array.
pub(crate) const LIST_WARNING_KINDS: &str = concat!(
    "SELECT op_id, code, MIN(rowid) AS first FROM op_warnings ",
    "WHERE op_id IN (SELECT value FROM json_each(?)) ",
    "GROUP BY op_id, code ORDER BY first"
);

/// Ends every running operation as failed (startup recovery), and nothing
/// else about it: not its times, warnings or target, and returns the ID of
/// each it ended, in no set order. Parameters: `status`, `error_code`,
/// `error_args`, `error_detail`, then the status it ends, running. A
/// running maintenance row stops being one, so the one-maintenance index
/// stops holding its repository.
pub(crate) const FAIL_RUNNING: &str = concat!(
    "UPDATE operations SET status = ?, error_code = ?, error_args = ?, error_detail = ? ",
    "WHERE status = ? RETURNING id"
);

/// The persistent queue (§5), oldest first: the queued operations by ID,
/// which is time order. The status is a literal, so SQLite matches the
/// statement to the partial index `ops_queue` when it prepares it, and the
/// read doesn't grow with the history.
pub(crate) const LIST_QUEUED: &str = select_operations!(" WHERE status = 'queued' ORDER BY id");

/// Every plan's planned run, in plan order.
pub(crate) const PLANNED_RUNS: &str =
    "SELECT plan_id, next_run FROM schedule_state ORDER BY plan_id";

/// Keeps a plan's planned run, replacing the one kept before. Parameters:
/// `plan_id`, `next_run`.
pub(crate) const PLAN_RUN: &str = concat!(
    "INSERT INTO schedule_state (plan_id, next_run) VALUES (?, ?) ",
    "ON CONFLICT(plan_id) DO UPDATE SET next_run = excluded.next_run"
);

/// Forgets a plan's planned run. Parameter: `plan_id`.
pub(crate) const UNPLAN: &str = "DELETE FROM schedule_state WHERE plan_id = ?";

/// Every plan's pending retry, in the order of the plans' IDs.
pub(crate) const PENDING_RETRIES: &str =
    "SELECT plan_id, attempt, retry_at, until FROM retry_state ORDER BY plan_id";

/// Keeps a plan's pending retry, replacing the one kept before. Parameters:
/// `plan_id`, `attempt`, `retry_at`, `until`.
pub(crate) const KEEP_RETRY: &str = concat!(
    "INSERT INTO retry_state (plan_id, attempt, retry_at, until) VALUES (?, ?, ?, ?) ",
    "ON CONFLICT(plan_id) DO UPDATE SET attempt = excluded.attempt, ",
    "retry_at = excluded.retry_at, until = excluded.until"
);

/// Forgets a plan's pending retry. Parameter: `plan_id`.
pub(crate) const DROP_RETRY: &str = "DELETE FROM retry_state WHERE plan_id = ?";

// History, newest first, in four statements: with or without a plan, and with
// or without a cursor. The cursor is a term of its own, not an `IS NULL OR`
// alternative, so a later page starts at its place in the index rather than
// reading every newer row first.

/// Parameter: how many rows.
pub(crate) const LIST_FIRST_PAGE: &str = select_operations!(" ORDER BY id DESC LIMIT ?");

/// Parameters: the cursor, how many rows.
pub(crate) const LIST_AFTER: &str = select_operations!(" WHERE id < ? ORDER BY id DESC LIMIT ?");

/// Parameters: the plan, how many rows.
pub(crate) const LIST_PLAN_FIRST_PAGE: &str =
    select_operations!(" WHERE plan_id = ? ORDER BY id DESC LIMIT ?");

/// Parameters: the plan, the cursor, how many rows.
pub(crate) const LIST_PLAN_AFTER: &str =
    select_operations!(" WHERE plan_id = ? AND id < ? ORDER BY id DESC LIMIT ?");

/// Whether any operation of a plan succeeded, with warnings or without.
/// Parameter: the plan. The statuses are literals, not parameters, so SQLite
/// can see when it prepares the statement that they are those of the partial
/// index `ops_succeeded_by_plan`, and answers from it: one probe, however
/// many operations the plan has failed.
pub(crate) const EVER_SUCCEEDED: &str = concat!(
    "SELECT EXISTS (SELECT 1 FROM operations ",
    "WHERE plan_id = ? AND status IN ('succeeded', 'succeeded_with_warnings'))"
);
