//! SQLite WAL state store (SPEC §3.1).
//!
//! All state transitions are persisted via SQLite with
//! `PRAGMA journal_mode = WAL;` and `PRAGMA synchronous = NORMAL;`.
//! The schema below is reproduced verbatim from the specification.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection, OptionalExtension, Row, Transaction};
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub mod views;

/// Relational schema, verbatim from SPEC §3.1.
pub const SCHEMA_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS tasks (
    id TEXT PRIMARY KEY,               -- UUIDv4 / short ulid
    title TEXT NOT NULL,
    prompt TEXT NOT NULL,
    base_commit TEXT NOT NULL,         -- SHA-1 or SHA-256 commit hash
    base_branch TEXT NOT NULL,
    strategy TEXT NOT NULL,            -- 'competing' | 'decomposed' | 'speculative'
    status TEXT NOT NULL,              -- 'PENDING'|'RUNNING'|'EVALUATING'|'COMPLETED'|'ABORTED'
    winner_candidate_id TEXT,
    created_at INTEGER NOT NULL,       -- Epoch milliseconds
    updated_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS candidates (
    id TEXT PRIMARY KEY,
    task_id TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
    variant_name TEXT NOT NULL,        -- e.g. "sonnet-minimal", "deepseek-rewrite"
    worktree_path TEXT NOT NULL,       -- Absolute path
    branch_name TEXT NOT NULL,         -- "ghosttree/task-id/variant-name"
    agent_cmd TEXT NOT NULL,           -- Executable string
    pid INTEGER,
    status TEXT NOT NULL,              -- 'INITIALIZING'|'WORKING'|'TESTING'|'READY'|'FAILED'|'MERGED'
    head_commit TEXT,                  -- Resulting commit SHA
    exit_code INTEGER,
    score REAL DEFAULT 0.0,
    test_results_json TEXT,            -- { "passed": 12, "failed": 0, "duration_ms": 420 }
    diff_stat_json TEXT,               -- { "files_changed": 4, "insertions": 110, "deletions": 12 }
    started_at INTEGER,
    finished_at INTEGER
);

CREATE TABLE IF NOT EXISTS candidate_events (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    candidate_id TEXT NOT NULL REFERENCES candidates(id) ON DELETE CASCADE,
    event_type TEXT NOT NULL,          -- 'SYSCALL'|'GIT_COMMIT'|'TEST_RUN'|'AGENT_STDOUT'
    payload TEXT NOT NULL,
    timestamp INTEGER NOT NULL
);
"#;

#[derive(Debug, Error)]
pub enum DbError {
    #[error("database error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("unknown task status stored in database: {0}")]
    UnknownTaskStatus(String),
    #[error("unknown task strategy stored in database: {0}")]
    UnknownTaskStrategy(String),
    #[error("unknown candidate status stored in database: {0}")]
    UnknownCandidateStatus(String),
    #[error("unknown candidate event type stored in database: {0}")]
    UnknownEventType(String),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
}

pub type Result<T> = std::result::Result<T, DbError>;

/// Current epoch time in milliseconds (SPEC timestamp convention).
pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Generate a new ULID for tasks / candidates.
pub fn new_id() -> String {
    ulid::Ulid::new().to_string()
}

/// Display form of an id: first 7 characters of the canonical ULID.
pub fn short_id(id: &str) -> String {
    id.chars().take(7).collect()
}

// ---------------------------------------------------------------------------
// Enumerated status values (stored as TEXT, validated on read).
// ---------------------------------------------------------------------------

macro_rules! text_enum {
    (
        $(#[$meta:meta])*
        $name:ident { $( $variant:ident => $text:literal ),+ $(,)? }
        $unknown_err:ident
    ) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub enum $name { $( $variant ),+ }

        impl $name {
            pub const ALL: &'static [Self] = &[ $( $name::$variant ),+ ];

            pub fn as_str(self) -> &'static str {
                match self { $( $name::$variant => $text ),+ }
            }

            pub fn from_db(raw: &str) -> Result<Self> {
                match raw {
                    $( $text => Ok($name::$variant), )+
                    other => Err(DbError::$unknown_err(other.to_string())),
                }
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(self.as_str())
            }
        }
    };
}

text_enum! {
    /// Task lifecycle status (SPEC §3.1 `tasks.status`).
    TaskStatus {
        Pending => "PENDING",
        Running => "RUNNING",
        Evaluating => "EVALUATING",
        Completed => "COMPLETED",
        Aborted => "ABORTED",
    }
    UnknownTaskStatus
}

impl TaskStatus {
    /// Terminal statuses hidden from `gt list` unless `--all` is passed.
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Aborted)
    }
}

text_enum! {
    /// Candidate lifecycle status (SPEC §3.1 `candidates.status`, §3.2 state machine).
    CandidateStatus {
        Initializing => "INITIALIZING",
        Working => "WORKING",
        Testing => "TESTING",
        Ready => "READY",
        Failed => "FAILED",
        Merged => "MERGED",
    }
    UnknownCandidateStatus
}

text_enum! {
    /// Swarm strategy (SPEC §3.1 `tasks.strategy`).
    TaskStrategy {
        Competing => "competing",
        Decomposed => "decomposed",
        Speculative => "speculative",
    }
    UnknownTaskStrategy
}

text_enum! {
    /// Candidate event classification (SPEC §3.1 `candidate_events.event_type`).
    EventType {
        Syscall => "SYSCALL",
        GitCommit => "GIT_COMMIT",
        TestRun => "TEST_RUN",
        AgentStdout => "AGENT_STDOUT",
    }
    UnknownEventType
}

/// Parsed form of `candidates.test_results_json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TestResults {
    pub passed: u32,
    pub failed: u32,
    pub duration_ms: u64,
}

/// Parsed form of `candidates.diff_stat_json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DiffStat {
    pub files_changed: u32,
    pub insertions: u64,
    pub deletions: u64,
}

// ---------------------------------------------------------------------------
// Row structs and insert inputs.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct NewTask {
    pub id: String,
    pub title: String,
    pub prompt: String,
    pub base_commit: String,
    pub base_branch: String,
    pub strategy: TaskStrategy,
}

#[derive(Debug)]
pub struct Task {
    pub id: String,
    pub title: String,
    pub prompt: String,
    pub base_commit: String,
    pub base_branch: String,
    pub strategy: Result<TaskStrategy>,
    pub status: Result<TaskStatus>,
    pub winner_candidate_id: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone)]
pub struct NewCandidate {
    pub id: String,
    pub task_id: String,
    pub variant_name: String,
    pub worktree_path: String,
    pub branch_name: String,
    pub agent_cmd: String,
}

#[derive(Debug)]
pub struct Candidate {
    pub id: String,
    pub task_id: String,
    pub variant_name: String,
    pub worktree_path: String,
    pub branch_name: String,
    pub agent_cmd: String,
    pub pid: Option<i64>,
    pub status: Result<CandidateStatus>,
    pub head_commit: Option<String>,
    pub exit_code: Option<i32>,
    pub score: f64,
    pub test_results_json: Option<String>,
    pub diff_stat_json: Option<String>,
    pub started_at: Option<i64>,
    pub finished_at: Option<i64>,
}

#[derive(Debug)]
pub struct CandidateEvent {
    pub id: i64,
    pub candidate_id: String,
    pub event_type: Result<EventType>,
    pub payload: String,
    pub timestamp: i64,
}

impl Candidate {
    /// Deserialize the stored test results, if any.
    pub fn test_results(&self) -> Result<Option<TestResults>> {
        match &self.test_results_json {
            Some(json) => Ok(Some(serde_json::from_str(json)?)),
            None => Ok(None),
        }
    }

    /// Deserialize the stored diff stat, if any.
    pub fn diff_stat(&self) -> Result<Option<DiffStat>> {
        match &self.diff_stat_json {
            Some(json) => Ok(Some(serde_json::from_str(json)?)),
            None => Ok(None),
        }
    }
}

// ---------------------------------------------------------------------------
// Store.
// ---------------------------------------------------------------------------

/// Handle over the SQLite connection holding all GhostTree state.
///
/// `rusqlite::Connection` is `Send` but not `Sync`; callers that need shared
/// access (the Phase 3 daemon) should wrap the store in a mutex.
pub struct Store {
    conn: Connection,
}

impl Store {
    /// Open (creating if needed) a store backed by `path`, configuring the
    /// WAL pragmas and applying the §3.1 schema.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let conn = Connection::open(path)?;
        Self::configure(conn)
    }

    /// In-memory store, for unit tests.
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        Self::configure(conn)
    }

    fn configure(conn: Connection) -> Result<Self> {
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        // Required for the schema's `ON DELETE CASCADE` to take effect.
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.busy_timeout(std::time::Duration::from_millis(5_000))?;
        conn.execute_batch(SCHEMA_SQL)?;
        Ok(Self { conn })
    }

    /// The active journal mode (useful for asserting WAL in tests and `gt list`).
    pub fn journal_mode(&self) -> Result<String> {
        self.conn
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .map_err(Into::into)
    }

    /// Begin an explicit transaction (rolled back on drop unless committed).
    pub fn transaction(&mut self) -> Result<Transaction<'_>> {
        self.conn.transaction().map_err(Into::into)
    }

    // -- tasks --------------------------------------------------------------

    pub fn insert_task(&self, task: &NewTask) -> Result<()> {
        let now = now_ms();
        self.conn.execute(
            "INSERT INTO tasks (id, title, prompt, base_commit, base_branch, strategy, status, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'PENDING', ?7, ?7)",
            params![
                task.id,
                task.title,
                task.prompt,
                task.base_commit,
                task.base_branch,
                task.strategy.as_str(),
                now,
            ],
        )?;
        Ok(())
    }

    pub fn get_task(&self, id: &str) -> Result<Option<Task>> {
        self.conn
            .query_row(
                "SELECT * FROM tasks WHERE id = ?1",
                params![id],
                row_to_task,
            )
            .optional()
            .map_err(Into::into)
    }

    /// List tasks; terminal ones (COMPLETED/ABORTED) only with `include_terminal`.
    pub fn list_tasks(&self, include_terminal: bool) -> Result<Vec<Task>> {
        let sql = if include_terminal {
            "SELECT * FROM tasks ORDER BY created_at"
        } else {
            "SELECT * FROM tasks WHERE status IN ('PENDING', 'RUNNING', 'EVALUATING') ORDER BY created_at"
        };
        let mut stmt = self.conn.prepare(sql)?;
        let rows = stmt
            .query_map([], row_to_task)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn update_task_status(&self, id: &str, status: TaskStatus) -> Result<usize> {
        self.conn
            .execute(
                "UPDATE tasks SET status = ?2, updated_at = ?3 WHERE id = ?1",
                params![id, status.as_str(), now_ms()],
            )
            .map_err(Into::into)
    }

    pub fn set_task_winner(&self, id: &str, candidate_id: &str) -> Result<usize> {
        self.conn
            .execute(
                "UPDATE tasks SET winner_candidate_id = ?2, updated_at = ?3 WHERE id = ?1",
                params![id, candidate_id, now_ms()],
            )
            .map_err(Into::into)
    }

    /// Delete a task; cascades to candidates and candidate_events.
    pub fn delete_task(&self, id: &str) -> Result<usize> {
        self.conn
            .execute("DELETE FROM tasks WHERE id = ?1", params![id])
            .map_err(Into::into)
    }

    // -- candidates ---------------------------------------------------------

    pub fn insert_candidate(&self, c: &NewCandidate) -> Result<()> {
        self.conn.execute(
            "INSERT INTO candidates (id, task_id, variant_name, worktree_path, branch_name, agent_cmd, status)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'INITIALIZING')",
            params![c.id, c.task_id, c.variant_name, c.worktree_path, c.branch_name, c.agent_cmd],
        )?;
        Ok(())
    }

    pub fn get_candidate(&self, id: &str) -> Result<Option<Candidate>> {
        self.conn
            .query_row(
                "SELECT * FROM candidates WHERE id = ?1",
                params![id],
                row_to_candidate,
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn list_candidates(&self, task_id: &str) -> Result<Vec<Candidate>> {
        let mut stmt = self
            .conn
            .prepare("SELECT * FROM candidates WHERE task_id = ?1 ORDER BY variant_name")?;
        let rows = stmt
            .query_map(params![task_id], row_to_candidate)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn set_candidate_status(&self, id: &str, status: CandidateStatus) -> Result<usize> {
        self.conn
            .execute(
                "UPDATE candidates SET status = ?2 WHERE id = ?1",
                params![id, status.as_str()],
            )
            .map_err(Into::into)
    }

    /// Candidates across every task (boot reconciliation scans this).
    pub fn list_all_candidates(&self) -> Result<Vec<Candidate>> {
        let mut stmt = self.conn.prepare("SELECT * FROM candidates ORDER BY id")?;
        let rows = stmt
            .query_map([], row_to_candidate)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Mark a candidate as live: records pid, starts the clock (`started_at`)
    /// and performs INITIALIZING -> WORKING (SPEC §3.2).
    pub fn start_candidate(&self, id: &str, pid: i64) -> Result<usize> {
        self.conn
            .execute(
                "UPDATE candidates SET pid = ?2, started_at = ?3, status = 'WORKING' WHERE id = ?1",
                params![id, pid, now_ms()],
            )
            .map_err(Into::into)
    }

    pub fn set_candidate_pid(&self, id: &str, pid: Option<i64>) -> Result<usize> {
        self.conn
            .execute(
                "UPDATE candidates SET pid = ?2 WHERE id = ?1",
                params![id, pid],
            )
            .map_err(Into::into)
    }

    pub fn set_candidate_head(&self, id: &str, sha: &str) -> Result<usize> {
        self.conn
            .execute(
                "UPDATE candidates SET head_commit = ?2 WHERE id = ?1",
                params![id, sha],
            )
            .map_err(Into::into)
    }

    /// Record process completion: exit code plus `finished_at` timestamp.
    pub fn finish_candidate(&self, id: &str, exit_code: Option<i32>) -> Result<usize> {
        self.conn
            .execute(
                "UPDATE candidates SET exit_code = ?2, finished_at = ?3 WHERE id = ?1",
                params![id, exit_code, now_ms()],
            )
            .map_err(Into::into)
    }

    pub fn set_candidate_score(&self, id: &str, score: f64) -> Result<usize> {
        self.conn
            .execute(
                "UPDATE candidates SET score = ?2 WHERE id = ?1",
                params![id, score],
            )
            .map_err(Into::into)
    }

    pub fn set_candidate_test_results(&self, id: &str, results: &TestResults) -> Result<usize> {
        let json = serde_json::to_string(results)?;
        self.conn
            .execute(
                "UPDATE candidates SET test_results_json = ?2 WHERE id = ?1",
                params![id, json],
            )
            .map_err(Into::into)
    }

    pub fn set_candidate_diff_stat(&self, id: &str, stat: &DiffStat) -> Result<usize> {
        let json = serde_json::to_string(stat)?;
        self.conn
            .execute(
                "UPDATE candidates SET diff_stat_json = ?2 WHERE id = ?1",
                params![id, json],
            )
            .map_err(Into::into)
    }

    // -- candidate events -----------------------------------------------------

    pub fn append_event(
        &self,
        candidate_id: &str,
        event_type: EventType,
        payload: &str,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO candidate_events (candidate_id, event_type, payload, timestamp)
             VALUES (?1, ?2, ?3, ?4)",
            params![candidate_id, event_type.as_str(), payload, now_ms()],
        )?;
        Ok(())
    }

    pub fn list_events(&self, candidate_id: &str) -> Result<Vec<CandidateEvent>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, candidate_id, event_type, payload, timestamp
             FROM candidate_events WHERE candidate_id = ?1 ORDER BY id",
        )?;
        let rows = stmt
            .query_map(params![candidate_id], row_to_event)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }
}

// ---------------------------------------------------------------------------
// Row mappers.
// ---------------------------------------------------------------------------

fn row_to_task(row: &Row<'_>) -> std::result::Result<Task, rusqlite::Error> {
    let strategy: String = row.get("strategy")?;
    let status: String = row.get("status")?;
    Ok(Task {
        id: row.get("id")?,
        title: row.get("title")?,
        prompt: row.get("prompt")?,
        base_commit: row.get("base_commit")?,
        base_branch: row.get("base_branch")?,
        strategy: TaskStrategy::from_db(&strategy),
        status: TaskStatus::from_db(&status),
        winner_candidate_id: row.get("winner_candidate_id")?,
        created_at: row.get("created_at")?,
        updated_at: row.get("updated_at")?,
    })
}

fn row_to_candidate(row: &Row<'_>) -> std::result::Result<Candidate, rusqlite::Error> {
    let status: String = row.get("status")?;
    let exit_code: Option<i64> = row.get("exit_code")?;
    Ok(Candidate {
        id: row.get("id")?,
        task_id: row.get("task_id")?,
        variant_name: row.get("variant_name")?,
        worktree_path: row.get("worktree_path")?,
        branch_name: row.get("branch_name")?,
        agent_cmd: row.get("agent_cmd")?,
        pid: row.get("pid")?,
        status: CandidateStatus::from_db(&status),
        head_commit: row.get("head_commit")?,
        exit_code: exit_code.map(|c| c as i32),
        score: row.get("score")?,
        test_results_json: row.get("test_results_json")?,
        diff_stat_json: row.get("diff_stat_json")?,
        started_at: row.get("started_at")?,
        finished_at: row.get("finished_at")?,
    })
}

fn row_to_event(row: &Row<'_>) -> std::result::Result<CandidateEvent, rusqlite::Error> {
    let event_type: String = row.get("event_type")?;
    Ok(CandidateEvent {
        id: row.get("id")?,
        candidate_id: row.get("candidate_id")?,
        event_type: EventType::from_db(&event_type),
        payload: row.get("payload")?,
        timestamp: row.get("timestamp")?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_task(id: &str) -> NewTask {
        NewTask {
            id: id.to_string(),
            title: "Fix race condition".into(),
            prompt: "Fix the race and add a stress test".into(),
            base_commit: "7b31cf2".into(),
            base_branch: "main".into(),
            strategy: TaskStrategy::Competing,
        }
    }

    fn sample_candidate(task_id: &str, id: &str) -> NewCandidate {
        NewCandidate {
            id: id.to_string(),
            task_id: task_id.to_string(),
            variant_name: "c1".into(),
            worktree_path: "/tmp/wt".into(),
            branch_name: "ghosttree/task/c1".into(),
            agent_cmd: "agent --run".into(),
        }
    }

    #[test]
    fn schema_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gt.db");
        {
            let store = Store::open(&path).unwrap();
            store.insert_task(&sample_task("t1")).unwrap();
        }
        // Re-opening (and thus re-applying SCHEMA_SQL) must not fail or lose data.
        let store = Store::open(&path).unwrap();
        assert!(store.get_task("t1").unwrap().is_some());
    }

    #[test]
    fn task_roundtrip_and_terminal_filter() {
        let store = Store::open_in_memory().unwrap();
        store.insert_task(&sample_task("t1")).unwrap();
        store.insert_task(&sample_task("t2")).unwrap();
        store
            .update_task_status("t2", TaskStatus::Completed)
            .unwrap();

        let all = store.list_tasks(true).unwrap();
        let active = store.list_tasks(false).unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].id, "t1");

        let task = store.get_task("t1").unwrap().unwrap();
        assert_eq!(task.title, "Fix race condition");
        assert_eq!(task.status.unwrap(), TaskStatus::Pending);
        assert_eq!(task.strategy.unwrap(), TaskStrategy::Competing);
        assert_eq!(task.winner_candidate_id, None);
        assert!(task.created_at > 0 && task.updated_at >= task.created_at);
    }

    #[test]
    fn unknown_status_read_is_rejected() {
        let store = Store::open_in_memory().unwrap();
        store.insert_task(&sample_task("t1")).unwrap();
        store
            .conn
            .execute("UPDATE tasks SET status = 'BOGUS'", [])
            .unwrap();
        match store.get_task("t1").unwrap() {
            Some(t) => assert!(matches!(t.status, Err(DbError::UnknownTaskStatus(_)))),
            None => panic!("task missing"),
        }
    }

    #[test]
    fn foreign_keys_are_enforced() {
        let store = Store::open_in_memory().unwrap();
        let fk: i64 = store
            .conn
            .query_row("PRAGMA foreign_keys", [], |r| r.get(0))
            .unwrap();
        assert_eq!(fk, 1);
        let err = store.insert_candidate(&sample_candidate("no-such-task", "c1"));
        assert!(matches!(err, Err(DbError::Sqlite(_))));
    }

    #[test]
    fn delete_task_cascades() {
        let store = Store::open_in_memory().unwrap();
        store.insert_task(&sample_task("t1")).unwrap();
        store
            .insert_candidate(&sample_candidate("t1", "c1"))
            .unwrap();
        store
            .insert_candidate(&sample_candidate("t1", "c2"))
            .unwrap();
        store
            .append_event("c1", EventType::AgentStdout, "hello")
            .unwrap();
        store
            .append_event("c1", EventType::GitCommit, "abc")
            .unwrap();

        assert_eq!(store.delete_task("t1").unwrap(), 1);
        assert!(store.get_task("t1").unwrap().is_none());
        assert!(store.get_candidate("c1").unwrap().is_none());
        assert!(store.get_candidate("c2").unwrap().is_none());
        assert!(store.list_events("c1").unwrap().is_empty());
    }

    #[test]
    fn candidate_column_updates_roundtrip() {
        let store = Store::open_in_memory().unwrap();
        store.insert_task(&sample_task("t1")).unwrap();
        store
            .insert_candidate(&sample_candidate("t1", "c1"))
            .unwrap();

        store.set_candidate_pid("c1", Some(41920)).unwrap();
        store
            .set_candidate_status("c1", CandidateStatus::Working)
            .unwrap();
        store.set_candidate_head("c1", "e4f810a").unwrap();
        store
            .set_candidate_test_results(
                "c1",
                &TestResults {
                    passed: 12,
                    failed: 0,
                    duration_ms: 420,
                },
            )
            .unwrap();
        store
            .set_candidate_diff_stat(
                "c1",
                &DiffStat {
                    files_changed: 4,
                    insertions: 110,
                    deletions: 12,
                },
            )
            .unwrap();
        store.set_candidate_score("c1", 96.4).unwrap();
        store.finish_candidate("c1", Some(0)).unwrap();

        let c = store.get_candidate("c1").unwrap().unwrap();
        assert_eq!(c.pid, Some(41920));
        assert_eq!(c.status.as_ref().unwrap(), &CandidateStatus::Working);
        assert_eq!(c.head_commit.as_deref(), Some("e4f810a"));
        assert_eq!(c.exit_code, Some(0));
        assert_eq!(c.score, 96.4);
        assert!(c.finished_at.is_some());
        assert_eq!(
            c.test_results().unwrap().unwrap(),
            TestResults {
                passed: 12,
                failed: 0,
                duration_ms: 420
            }
        );
        assert_eq!(
            c.diff_stat().unwrap().unwrap(),
            DiffStat {
                files_changed: 4,
                insertions: 110,
                deletions: 12
            }
        );
    }

    #[test]
    fn events_list_in_insertion_order() {
        let store = Store::open_in_memory().unwrap();
        store.insert_task(&sample_task("t1")).unwrap();
        store
            .insert_candidate(&sample_candidate("t1", "c1"))
            .unwrap();
        store.append_event("c1", EventType::Syscall, "one").unwrap();
        store.append_event("c1", EventType::TestRun, "two").unwrap();
        let events = store.list_events("c1").unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].payload, "one");
        assert_eq!(events[1].event_type.as_ref().unwrap(), &EventType::TestRun);
        assert!(events.iter().all(|e| e.timestamp > 0));
    }

    #[test]
    fn transaction_rollback_discards_writes() {
        let mut store = Store::open_in_memory().unwrap();
        {
            let tx = store.transaction().unwrap();
            tx.execute(
                "INSERT INTO tasks (id, title, prompt, base_commit, base_branch, strategy, status, created_at, updated_at)
                 VALUES ('tx1', 't', 'p', 'abc', 'main', 'competing', 'PENDING', 1, 1)",
                [],
            )
            .unwrap();
            let count: i64 = tx
                .query_row("SELECT COUNT(*) FROM tasks", [], |r| r.get(0))
                .unwrap();
            assert_eq!(count, 1);
            tx.rollback().unwrap();
        }
        assert!(store.list_tasks(true).unwrap().is_empty());
    }
}
