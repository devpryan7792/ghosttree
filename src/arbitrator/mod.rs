//! Arbitrator & evaluation engine (SPEC §7).
//!
//! Responsibilities:
//! * deterministic test-runner execution with a hard timeout (shared by the
//!   daemon's grading path and `gt eval --re-run`),
//! * recovery of `test_results_json` counters from harness output,
//! * diff statistics via `git diff --numstat` plumbing (`diff_stat_json`),
//! * multi-criteria scoring (`score.rs`) and winner selection persisted onto
//!   the task (`winner_candidate_id` + status COMPLETED).

pub mod score;

use std::fs::File;
use std::io::Read;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::db::{CandidateStatus, DiffStat, EventType, Store, TaskStatus, TestResults};
use crate::git::Repo;
use crate::supervisor::proc::ProcessGroup;

pub use score::{parse_test_output, results_from_output, ScoreBreakdown, ScoreInputs, Weights};

/// Hard ceiling for one test-harness execution (SPEC §7 arbitrator budget).
pub const TEST_TIMEOUT: Duration = Duration::from_secs(300);
/// Grace before SIGKILL for the test command group.
pub const TEST_GRACE: Duration = Duration::from_secs(2);
/// Poll cadence while the harness runs.
const TEST_POLL: Duration = Duration::from_millis(50);

/// Result of one completed test-harness run.
#[derive(Debug, Clone)]
pub struct TestRun {
    pub exit_code: Option<i32>,
    pub success: bool,
    pub timed_out: bool,
    pub duration: Duration,
    pub log: std::path::PathBuf,
}

/// Run a task's test command inside a candidate worktree with a hard
/// timeout. Output lands in `log_path` (O_APPEND interleaving of
/// stdout/stderr). The command runs in its own process group so the ceiling
/// kills the whole tree (SPEC §6.1).
pub fn run_test_cmd(worktree: &Path, test_cmd: &str, log_path: &Path) -> Result<TestRun, String> {
    if let Some(parent) = log_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("test log dir create failed: {e}"))?;
    }
    let out_file = File::create(log_path).map_err(|e| format!("test log create failed: {e}"))?;
    let err_file = File::options()
        .append(true)
        .open(log_path)
        .map_err(|e| format!("test log create failed: {e}"))?;

    let mut cmd = Command::new("sh");
    cmd.arg("-c")
        .arg(test_cmd)
        .current_dir(worktree)
        .stdin(Stdio::null())
        .stdout(Stdio::from(out_file))
        .stderr(Stdio::from(err_file))
        .process_group(0);

    let mut group =
        ProcessGroup::spawn(cmd).map_err(|e| format!("test command spawn failed: {e}"))?;

    let started = Instant::now();
    let deadline = started + TEST_TIMEOUT;
    let mut timed_out = false;
    let status = loop {
        match group.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(e) => {
                // Reap failed: escalate to the kill ladder for safety, then
                // report the harness itself as broken.
                let _ = group.terminate(TEST_GRACE);
                return Err(format!("test command poll failed: {e}"));
            }
        }
        if Instant::now() >= deadline {
            timed_out = true;
            let status = group
                .terminate(TEST_GRACE)
                .map_err(|e| format!("test kill failed: {e}"))?;
            break status;
        }
        std::thread::sleep(TEST_POLL);
    };

    Ok(TestRun {
        exit_code: status.code(),
        success: status.success() && !timed_out,
        timed_out,
        duration: started.elapsed(),
        log: log_path.to_path_buf(),
    })
}

/// Read a capture log and distill the persisted `test_results_json` counters
/// from it (falls back to the one-implicit-smoke-test convention).
pub fn results_from_log(log_path: &Path, succeeded: bool, duration_ms: u64) -> TestResults {
    let text = File::open(log_path)
        .and_then(|mut f| {
            let mut buf = String::new();
            f.read_to_string(&mut buf)?;
            Ok(buf)
        })
        .map(|buf| String::from_utf8_lossy(buf.as_bytes()).into_owned())
        .unwrap_or_default();
    results_from_output(parse_test_output(&text), succeeded, duration_ms)
}

/// Committed diff statistics for a candidate worktree against its task base
/// commit. `Ok(None)` when the worktree is gone or has no committed work —
/// the caller treats that as zero churn.
pub fn compute_diff_stat(worktree: &Path, base_commit: &str) -> Result<Option<DiffStat>, String> {
    if !worktree.is_dir() {
        return Ok(None);
    }
    let repo = Repo::discover(worktree).map_err(|e| format!("worktree discovery failed: {e}"))?;
    let head = Repo::worktree_head(worktree).map_err(|e| format!("worktree HEAD failed: {e}"))?;
    if head == base_commit {
        return Ok(Some(DiffStat {
            files_changed: 0,
            insertions: 0,
            deletions: 0,
        }));
    }
    let stat = repo
        .diff_numstat(base_commit, "HEAD")
        .map_err(|e| format!("diff numstat failed: {e}"))?;
    Ok(Some(DiffStat {
        files_changed: stat.files_changed,
        insertions: stat.insertions,
        deletions: stat.deletions,
    }))
}

// ---------------------------------------------------------------------------
// Task evaluation (SPEC §7 scoring matrix + winner selection).
// ---------------------------------------------------------------------------

/// One candidate's post-evaluation summary.
#[derive(Debug, Clone, PartialEq)]
pub struct ScoredCandidate {
    pub candidate_id: String,
    pub variant_name: String,
    pub status: String,
    pub score: f64,
    pub breakdown: ScoreBreakdown,
}

/// Outcome of evaluating one task.
#[derive(Debug, Clone, PartialEq)]
pub struct TaskEvaluation {
    pub task_id: String,
    pub winner_candidate_id: Option<String>,
    pub scored: Vec<ScoredCandidate>,
}

/// Score every terminal candidate of `task_id` and persist the winner.
///
/// * Candidates still owning a live run (INITIALIZING/WORKING/TESTING) make
///   evaluation impossible — the caller must wait for the daemon or refuse.
/// * Winner selection is restricted to READY (or already-MERGED) candidates;
///   FAILED runs can never be promoted (SPEC §3.2). An existing winner stays
///   on re-evaluation while it remains eligible.
/// * A task with no eligible candidate keeps its status so `gt eval --re-run`
///   can produce one later.
pub fn evaluate_task(store: &Store, repo: &Repo, task_id: &str) -> Result<TaskEvaluation, String> {
    let task = store
        .get_task(task_id)
        .map_err(|e| format!("state store error: {e}"))?
        .ok_or_else(|| format!("unknown task {task_id:?}"))?;

    let candidates = store
        .list_candidates(task_id)
        .map_err(|e| format!("state store error: {e}"))?;
    for c in &candidates {
        if matches!(
            c.status.as_ref(),
            Ok(CandidateStatus::Initializing | CandidateStatus::Working | CandidateStatus::Testing)
        ) {
            return Err(format!(
                "candidate {} ({}) is still {}; evaluation requires a settled run",
                c.id,
                c.variant_name,
                c.status.as_ref().map(|s| s.as_str()).unwrap_or("?")
            ));
        }
    }

    let mut scored = Vec::with_capacity(candidates.len());
    for c in &candidates {
        let ready = matches!(
            c.status.as_ref(),
            Ok(CandidateStatus::Ready | CandidateStatus::Merged)
        );
        let test_results = c.test_results().unwrap_or(None);
        let diff_stat = c.diff_stat().unwrap_or(None);
        let duration_secs = match (c.started_at, c.finished_at) {
            (Some(start), Some(end)) => (end - start).max(0) as f64 / 1_000.0,
            _ => test_results
                .as_ref()
                .map(|r| r.duration_ms as f64 / 1_000.0)
                .unwrap_or(0.0),
        };
        let inputs = ScoreInputs {
            test_results: backfill_test_results(
                repo,
                task_id,
                c,
                test_results,
                ready,
                duration_secs,
            ),
            succeeded: ready,
            coverage_delta: None,
            diff_stat: backfill_diff_stat(repo, &task.base_commit, c, diff_stat),
            duration_secs,
        };
        let breakdown = ScoreBreakdown::compute(&inputs, &Weights::default());
        store
            .set_candidate_score(&c.id, breakdown.final_score)
            .map_err(|e| format!("state store error: {e}"))?;
        scored.push(ScoredCandidate {
            candidate_id: c.id.clone(),
            variant_name: c.variant_name.clone(),
            status: c
                .status
                .as_ref()
                .map(|s| s.as_str())
                .unwrap_or("?")
                .to_string(),
            score: breakdown.final_score,
            breakdown,
        });
    }

    // Winner: keep an existing eligible winner, else the best-scoring READY
    // candidate (deterministic tie-break on variant name).
    let eligible = |c: &crate::db::Candidate| {
        matches!(
            c.status.as_ref(),
            Ok(CandidateStatus::Ready | CandidateStatus::Merged)
        )
    };
    let winner = match &task.winner_candidate_id {
        Some(existing) if candidates.iter().any(|c| c.id == *existing && eligible(c)) => {
            Some(existing.clone())
        }
        _ => {
            let mut best: Option<(f64, &str, String)> = None;
            for c in candidates.iter().filter(|c| eligible(c)) {
                let s = scored
                    .iter()
                    .find(|sc| sc.candidate_id == c.id)
                    .map(|sc| sc.score)
                    .unwrap_or(f64::MIN);
                let better = match &best {
                    None => true,
                    Some((bs, _, _)) => s > *bs,
                };
                if better {
                    best = Some((s, c.variant_name.as_str(), c.id.clone()));
                }
            }
            best.map(|(_, _, id)| id)
        }
    };

    if let Some(winner_id) = &winner {
        store
            .set_task_winner(task_id, winner_id)
            .map_err(|e| format!("state store error: {e}"))?;
        store
            .update_task_status(task_id, TaskStatus::Completed)
            .map_err(|e| format!("state store error: {e}"))?;
    }

    Ok(TaskEvaluation {
        task_id: task_id.to_string(),
        winner_candidate_id: winner,
        scored,
    })
}

/// Recover a task's test command from the newest TEST_RUN event that carried
/// one (`gt eval --re-run` uses this when `--test-cmd` was not supplied).
pub fn recover_test_cmd(store: &Store, task_id: &str) -> Option<String> {
    for candidate in store.list_candidates(task_id).ok()? {
        for event in store.list_events(&candidate.id).ok()?.iter().rev() {
            if !matches!(event.event_type.as_ref(), Ok(EventType::TestRun)) {
                continue;
            }
            if let Ok(payload) = serde_json::from_str::<serde_json::Value>(&event.payload) {
                if let Some(cmd) = payload.get("cmd").and_then(|c| c.as_str()) {
                    if !cmd.trim().is_empty() {
                        return Some(cmd.to_string());
                    }
                }
            }
        }
    }
    None
}

/// Map a git [`crate::git::NumStat`] into the persisted [`DiffStat`] shape.
pub fn diff_stat_from_numstat(stat: crate::git::NumStat) -> DiffStat {
    DiffStat {
        files_changed: stat.files_changed,
        insertions: stat.insertions,
        deletions: stat.deletions,
    }
}

/// Fill in missing test counters from the candidate's harness capture log
/// (back-compat for state graded before counters were persisted).
fn backfill_test_results(
    repo: &Repo,
    task_id: &str,
    candidate: &crate::db::Candidate,
    stored: Option<TestResults>,
    ready: bool,
    duration_secs: f64,
) -> Option<TestResults> {
    if stored.is_some() {
        return stored;
    }
    let log = repo
        .root()
        .join(crate::daemon::GHOST_DIR)
        .join("logs")
        .join(task_id)
        .join(format!("{}.tests.log", candidate.id));
    if !log.is_file() {
        return None;
    }
    let duration_ms = (duration_secs * 1_000.0) as u64;
    Some(results_from_log(&log, ready, duration_ms))
}

/// Fill in missing diff statistics straight from git plumbing
/// (`git diff --numstat base..head`), e.g. for re-evaluation after the
/// daemon stored only partial data.
fn backfill_diff_stat(
    repo: &Repo,
    base_commit: &str,
    candidate: &crate::db::Candidate,
    stored: Option<DiffStat>,
) -> Option<DiffStat> {
    if stored.is_some() {
        return stored;
    }
    let head = candidate.head_commit.as_deref()?;
    repo.diff_numstat(base_commit, head)
        .ok()
        .map(diff_stat_from_numstat)
}
