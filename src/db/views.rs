//! Serializable row views shared by the CLI (`gt list`) and the daemon's
//! `GetStatus` RPC so both surfaces emit identical wire shapes.

use serde::Serialize;

use super::{Candidate, DbError, Store, Task};

#[derive(Debug, Clone, Serialize)]
pub struct CandidateView {
    pub id: String,
    pub task_id: String,
    pub variant_name: String,
    pub status: String,
    pub branch_name: String,
    pub worktree_path: String,
    pub agent_cmd: String,
    pub pid: Option<i64>,
    pub head_commit: Option<String>,
    pub exit_code: Option<i32>,
    pub score: f64,
    pub started_at: Option<i64>,
    pub finished_at: Option<i64>,
    pub test_results: Option<super::TestResults>,
    pub diff_stat: Option<super::DiffStat>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TaskView {
    pub id: String,
    pub title: String,
    pub base_branch: String,
    pub base_commit: String,
    pub strategy: String,
    pub status: String,
    pub winner_candidate_id: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
    pub candidates: Vec<CandidateView>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Snapshot {
    pub tasks: Vec<TaskView>,
}

impl CandidateView {
    /// Map a DB row; status decode failures render as the error text so a
    /// corrupted row is visible on the wire instead of dropped.
    pub fn from_row(c: &Candidate) -> Self {
        Self {
            id: c.id.clone(),
            task_id: c.task_id.clone(),
            variant_name: c.variant_name.clone(),
            status: enum_text(&c.status),
            branch_name: c.branch_name.clone(),
            worktree_path: c.worktree_path.clone(),
            agent_cmd: c.agent_cmd.clone(),
            pid: c.pid,
            head_commit: c.head_commit.clone(),
            exit_code: c.exit_code,
            score: c.score,
            started_at: c.started_at,
            finished_at: c.finished_at,
            test_results: c.test_results().ok().flatten(),
            diff_stat: c.diff_stat().ok().flatten(),
        }
    }
}

impl TaskView {
    pub fn from_row(t: &Task, candidates: Vec<CandidateView>) -> Self {
        Self {
            id: t.id.clone(),
            title: t.title.clone(),
            base_branch: t.base_branch.clone(),
            base_commit: t.base_commit.clone(),
            strategy: enum_text(&t.strategy),
            status: enum_text(&t.status),
            winner_candidate_id: t.winner_candidate_id.clone(),
            created_at: t.created_at,
            updated_at: t.updated_at,
            candidates,
        }
    }
}

fn enum_text<T: std::fmt::Display, E: std::fmt::Display>(value: &Result<T, E>) -> String {
    match value {
        Ok(v) => v.to_string(),
        Err(e) => e.to_string(),
    }
}

/// Build the full task/candidate snapshot.
pub fn snapshot(store: &Store, include_terminal: bool) -> Result<Snapshot, DbError> {
    let tasks = store.list_tasks(include_terminal)?;
    let mut views = Vec::with_capacity(tasks.len());
    for task in &tasks {
        let candidates = store
            .list_candidates(&task.id)?
            .iter()
            .map(CandidateView::from_row)
            .collect();
        views.push(TaskView::from_row(task, candidates));
    }
    Ok(Snapshot { tasks: views })
}
