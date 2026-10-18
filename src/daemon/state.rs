//! Candidate lifecycle state machine (SPEC §3.2) plus the bodies behind the
//! daemon's RPC surface (SPEC §6.2) and boot reconciliation (SPEC §8).
//!
//! Threading: `spawn_swarm` provisions worktrees and launches agents
//! synchronously (cheap git ops), answers the RPC, and hands each live
//! `Spawned` handle to a runner thread. The runner polls process exit,
//! kill requests, and commit progress (the SPEC §8 idle watchdog), then
//! drives WORKING -> TESTING -> READY/FAILED and broadcasts every
//! transition as a `CandidateStateChanged` notification.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::sync::mpsc::{channel, Receiver};
use std::sync::{Arc, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::json;

use crate::arbitrator;
use crate::daemon::ipc::SpawnSwarmParams;
use crate::daemon::{broadcast, DaemonShared, GHOST_DIR};
use crate::db::{
    new_id, now_ms, short_id, CandidateStatus, DbError, EventType, NewCandidate, NewTask, Store,
    TaskStatus, TaskStrategy,
};
use crate::git::Repo;
use crate::supervisor::{SpawnConfig, Spawned, TERMINATION_GRACE};

/// Runner poll cadence for process exit / kill signals (bounds
/// `KillCandidate` delivery latency).
const STATUS_POLL: Duration = Duration::from_millis(50);
/// Cadence for checking agent commit progress (`git rev-parse HEAD`).
const COMMIT_POLL: Duration = Duration::from_secs(3);
/// SPEC §8 default: runtime longer than this without a new commit is stalled.
const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(900);
/// SIGTERM wait for orphaned agent groups during boot reconciliation.
const ORPHAN_KILL_GRACE: Duration = Duration::from_secs(2);
/// First dev-server port handed to variant #0 (SPEC §8 isolation example).
const BASE_PORT: u16 = 3100;

pub(crate) type StateError = (i32, String);
pub(crate) type StateResult<T> = Result<T, StateError>;

/// Bad request parameters (-32602).
fn bad_params(message: impl Into<String>) -> StateError {
    (-32602, message.into())
}

/// Infrastructure failure inside a method (-32000, server-defined range).
fn server_error(message: impl Into<String>) -> StateError {
    (-32000, message.into())
}

fn db_to_state(e: DbError) -> StateError {
    (-32603, format!("state store error: {e}"))
}

/// Acquire the shared store lock; poisoning is an internal error.
fn store_lock(shared: &Arc<DaemonShared>) -> StateResult<MutexGuard<'_, Store>> {
    shared
        .store
        .lock()
        .map_err(|_| (-32603, "state store poisoned".to_string()))
}

/// Run one DB operation under the store lock, mapping errors to -32603.
fn db_store<T>(
    shared: &Arc<DaemonShared>,
    f: impl FnOnce(&Store) -> Result<T, DbError>,
) -> StateResult<T> {
    let store = store_lock(shared)?;
    f(&store).map_err(db_to_state)
}

// ---------------------------------------------------------------------------
// CandidateStateChanged notifications (SPEC §6.2).
// ---------------------------------------------------------------------------

pub(crate) const CANDIDATE_STATE_METHOD: &str = "CandidateStateChanged";

#[derive(Serialize)]
struct CandidateStateChange<'a> {
    candidate_id: &'a str,
    status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    head_commit: Option<&'a str>,
    timestamp: i64,
}

/// Broadcast one state transition to every connected RPC client.
pub(crate) fn notify_state(
    shared: &Arc<DaemonShared>,
    candidate_id: &str,
    status: CandidateStatus,
    head_commit: Option<&str>,
) {
    let params = serde_json::to_value(CandidateStateChange {
        candidate_id,
        status: status.as_str(),
        head_commit,
        timestamp: now_ms(),
    })
    .unwrap_or_else(
        |_| json!({"candidate_id": candidate_id, "status": status.as_str(), "timestamp": now_ms()}),
    );
    broadcast(shared, CANDIDATE_STATE_METHOD, params);
}

// ---------------------------------------------------------------------------
// SpawnSwarm (SPEC §6.2).
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub(crate) struct SpawnSwarmInfo {
    pub task_id: String,
    pub base_commit: String,
    pub base_branch: String,
    pub candidates: Vec<SpawnedCandidateInfo>,
}

#[derive(Debug, Serialize)]
pub(crate) struct SpawnedCandidateInfo {
    pub id: String,
    pub variant_name: String,
    pub agent_cmd: String,
    pub worktree_path: String,
    pub branch_name: String,
    pub pid: Option<i64>,
    pub status: String,
}

struct VariantPlan {
    name: String,
    agent_cmd: String,
}

/// What a runner thread needs to finish one candidate without referring back
/// to the RPC request that spawned it.
struct RunnerInput {
    task_id: String,
    candidate_id: String,
    worktree: PathBuf,
    test_cmd: Option<String>,
    timeout: Duration,
    base_commit: String,
    spawned: Spawned,
}

/// Worktree path under the repo: `.ghosttree/worktrees/<task-id>-<variant>`
/// (SPEC §4.1).
fn worktree_rel_path(task_id: &str, variant: &str) -> PathBuf {
    PathBuf::from(GHOST_DIR)
        .join("worktrees")
        .join(format!("{task_id}-{variant}"))
}

/// Branch hosting one candidate variant (SPEC §4.1).
fn branch_name(task_id: &str, variant: &str) -> String {
    format!("ghosttree/{task_id}/{variant}")
}

/// Variant names become path + ref components; keep the alphabet tight.
fn valid_variant_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && !name.starts_with('.')
        && !name.ends_with(".lock")
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// Task titles are display text: first non-empty line, whitespace-collapsed,
/// truncated at 80 characters.
fn title_from_prompt(prompt: &str) -> String {
    let line = prompt
        .split(['\n', '\r'])
        .find(|l| !l.trim().is_empty())
        .unwrap_or("unnamed task");
    let collapsed: String = line.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut title: String = collapsed.chars().take(80).collect();
    if collapsed.chars().count() > 80 {
        title.push_str("...");
    }
    title
}

/// Per-variant environment (SPEC §4.2/§8): offset dev-server ports and a
/// variant-local cargo target dir so parallel builds never share artifacts.
/// (`node_modules` hardlink farms land with the Phase 5 e2e surface.)
fn candidate_env(
    shared: &Arc<DaemonShared>,
    worktree: &Path,
    variant: &str,
    index: usize,
) -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    env.insert("PORT".to_string(), (BASE_PORT + index as u16).to_string());
    if worktree.join("Cargo.toml").is_file() {
        let target = shared
            .ghost_dir()
            .join("cache")
            .join("target")
            .join(variant);
        // Pre-create so the first cargo invocation never races mkdir.
        let _ = fs::create_dir_all(&target);
        env.insert("CARGO_TARGET_DIR".to_string(), target.display().to_string());
    }
    env
}

/// Log capture directory for one task: `.ghosttree/logs/<task-id>` (SPEC §6.1).
fn task_log_dir(shared: &Arc<DaemonShared>, task_id: &str) -> PathBuf {
    shared.ghost_dir().join("logs").join(task_id)
}

/// Capture log for a candidate's test harness run.
fn test_log_path(shared: &Arc<DaemonShared>, task_id: &str, candidate_id: &str) -> PathBuf {
    task_log_dir(shared, task_id).join(format!("{candidate_id}.tests.log"))
}

/// Terminal FAILED for a candidate with no runner thread: stamp
/// `finished_at`, record the reason, and broadcast.
fn finish_failed(shared: &Arc<DaemonShared>, candidate_id: &str, reason: String) {
    let _ = db_store(shared, |store| {
        store.append_event(candidate_id, EventType::Syscall, &reason)?;
        store.finish_candidate(candidate_id, None)?;
        store.set_candidate_status(candidate_id, CandidateStatus::Failed)
    });
    notify_state(shared, candidate_id, CandidateStatus::Failed, None);
}

/// True while a candidate still owns (or could re-open) agent work.
fn candidate_is_active(status: Result<&CandidateStatus, &DbError>) -> bool {
    matches!(
        status,
        Ok(CandidateStatus::Initializing | CandidateStatus::Working | CandidateStatus::Testing)
    )
}

/// Hand a task to evaluation once none of its candidates is active
/// (SPEC §3.2 run phase complete): mark EVALUATING, then let the arbitrator
/// score the candidates and pick the winner (SPEC §7). The close is only
/// triggered from PENDING/RUNNING so re-evaluation stays the `gt eval`
/// surface's job.
fn maybe_close_task(shared: &Arc<DaemonShared>, task_id: &str) {
    let closed = db_store(shared, |store| {
        let task = store.get_task(task_id)?;
        let task = match task {
            Some(task) => task,
            None => return Ok(false),
        };
        if !matches!(
            task.status.as_ref(),
            Ok(TaskStatus::Pending | TaskStatus::Running)
        ) {
            return Ok(false);
        }
        let candidates = store.list_candidates(task_id)?;
        if candidates
            .iter()
            .all(|c| !candidate_is_active(c.status.as_ref()))
        {
            store.update_task_status(task_id, TaskStatus::Evaluating)?;
            Ok(true)
        } else {
            Ok(false)
        }
    });
    match closed {
        Ok(true) => arbitrate(shared, task_id),
        Ok(false) => {}
        Err((_, message)) => eprintln!("[GhostTree] task close failed: {message}"),
    }
}

/// Run the SPEC §7 arbitrator for a settled task: scores land on the
/// candidate rows and the winner (if any) on the task. Failures are logged
/// and leave the task EVALUATING for a later `gt eval`.
fn arbitrate(shared: &Arc<DaemonShared>, task_id: &str) {
    let result: Result<arbitrator::TaskEvaluation, String> = (|| {
        let repo =
            Repo::discover(&shared.root).map_err(|e| format!("git discovery failed: {e}"))?;
        let store = store_lock(shared).map_err(|(_, message)| message)?;
        arbitrator::evaluate_task(&store, &repo, task_id)
    })();
    match result {
        Ok(evaluation) => match &evaluation.winner_candidate_id {
            Some(winner) => {
                let variant = evaluation
                    .scored
                    .iter()
                    .find(|sc| sc.candidate_id == *winner)
                    .map(|sc| sc.variant_name.clone())
                    .unwrap_or_default();
                let score = evaluation
                    .scored
                    .iter()
                    .find(|sc| sc.candidate_id == *winner)
                    .map(|sc| sc.score)
                    .unwrap_or(0.0);
                eprintln!(
                    "[GhostTree] task {task_id} evaluation complete: winner {winner} \
                     ({variant}, score {score:.1})"
                );
            }
            None => {
                eprintln!("[GhostTree] task {task_id} evaluation found no promotable candidate");
            }
        },
        Err(message) => eprintln!("[GhostTree] arbitration failed for {task_id}: {message}"),
    }
}

/// `SpawnSwarm` synchronous half: create the task + candidate rows, provision
/// one worktree per variant (SPEC §4.1), spawn each agent into its own PTY
/// and process group, then hand live handles to runner threads.
pub(crate) fn spawn_swarm(
    shared: &Arc<DaemonShared>,
    p: SpawnSwarmParams,
) -> StateResult<SpawnSwarmInfo> {
    if p.prompt.trim().is_empty() {
        return Err(bad_params("prompt must not be empty"));
    }
    if p.variants.is_empty() {
        return Err(bad_params("SpawnSwarm requires at least 1 variant"));
    }

    let test_cmd = p.test_cmd.clone();
    let timeout = Duration::from_secs(
        p.timeout_secs
            .unwrap_or_else(|| DEFAULT_IDLE_TIMEOUT.as_secs()),
    );

    let mut variants: Vec<VariantPlan> = Vec::with_capacity(p.variants.len());
    for (index, spec) in p.variants.into_iter().enumerate() {
        let agent_cmd = spec.agent_cmd.trim().to_string();
        if agent_cmd.is_empty() {
            return Err(bad_params(format!(
                "variants[{index}].agent_cmd must not be empty"
            )));
        }
        let name = match spec.name.as_deref().map(str::trim) {
            Some(name) if !name.is_empty() => name.to_string(),
            _ => format!("c{}", index + 1),
        };
        if !valid_variant_name(&name) {
            return Err(bad_params(format!(
                "variants[{index}].name {name:?} must be 1-64 chars of [A-Za-z0-9._-] and not start with '.'"
            )));
        }
        if variants.iter().any(|v| v.name == name) {
            return Err(bad_params(format!("duplicate variant name {name:?}")));
        }
        variants.push(VariantPlan { name, agent_cmd });
    }

    // Base geometry: everything branches from HEAD at swarm time (SPEC §4.1).
    let repo = Repo::discover(&shared.root)
        .map_err(|e| server_error(format!("git discovery failed: {e}")))?;
    let base_commit = repo
        .head_commit()
        .map_err(|e| server_error(format!("git HEAD lookup failed: {e}")))?;
    let base_branch = repo
        .current_branch()
        .map_err(|e| server_error(format!("git branch lookup failed: {e}")))?
        .unwrap_or_else(|| "(detached)".to_string());

    let task_id = p.task_id.unwrap_or_else(new_id);

    // One transaction: task row + every candidate row (all INITIALIZING).
    let candidate_ids: Vec<String> = {
        let store = store_lock(shared)?;
        if store.get_task(&task_id).map_err(db_to_state)?.is_some() {
            return Err(bad_params(format!("task {task_id:?} already exists")));
        }
        store
            .insert_task(&NewTask {
                id: task_id.clone(),
                title: title_from_prompt(&p.prompt),
                prompt: p.prompt,
                base_commit: base_commit.clone(),
                base_branch: base_branch.clone(),
                strategy: TaskStrategy::Competing,
            })
            .map_err(db_to_state)?;
        let ids: Vec<String> = variants.iter().map(|_| new_id()).collect();
        for (plan, id) in variants.iter().zip(&ids) {
            store
                .insert_candidate(&NewCandidate {
                    id: id.clone(),
                    task_id: task_id.clone(),
                    variant_name: plan.name.clone(),
                    worktree_path: worktree_rel_path(&task_id, &plan.name)
                        .display()
                        .to_string(),
                    branch_name: branch_name(&task_id, &plan.name),
                    agent_cmd: plan.agent_cmd.clone(),
                })
                .map_err(db_to_state)?;
        }
        ids
    };

    // Announce every candidate as INITIALIZING (SPEC §6.2 notifications).
    for id in &candidate_ids {
        notify_state(shared, id, CandidateStatus::Initializing, None);
    }

    let log_dir = task_log_dir(shared, &task_id);
    let mut infos = Vec::with_capacity(candidate_ids.len());

    for (index, (plan, candidate_id)) in variants.iter().zip(&candidate_ids).enumerate() {
        let rel = worktree_rel_path(&task_id, &plan.name);
        let branch = branch_name(&task_id, &plan.name);
        let info_of = |status: CandidateStatus, pid: Option<i64>| SpawnedCandidateInfo {
            id: candidate_id.clone(),
            variant_name: plan.name.clone(),
            agent_cmd: plan.agent_cmd.clone(),
            worktree_path: rel.display().to_string(),
            branch_name: branch.clone(),
            pid,
            status: status.as_str().to_string(),
        };

        match repo.worktree_add(&branch, &rel, &base_commit) {
            Ok(worktree) => {
                let mut cfg = SpawnConfig::new(
                    task_id.clone(),
                    candidate_id.clone(),
                    plan.agent_cmd.clone(),
                    worktree.clone(),
                    log_dir.clone(),
                );
                cfg.env = candidate_env(shared, &worktree, &plan.name, index);

                match crate::supervisor::spawn(&cfg) {
                    Ok(spawned) => {
                        let pid = spawned.pid() as i64;
                        if let Err(e) =
                            db_store(shared, |store| store.start_candidate(candidate_id, pid))
                        {
                            // Agent is live but unrecorded; the runner owns
                            // its lifecycle from here regardless.
                            eprintln!(
                                "[GhostTree] failed to record start of {candidate_id}: {}",
                                e.1
                            );
                        }
                        notify_state(shared, candidate_id, CandidateStatus::Working, None);
                        let input = RunnerInput {
                            task_id: task_id.clone(),
                            candidate_id: candidate_id.clone(),
                            worktree,
                            test_cmd: test_cmd.clone(),
                            timeout,
                            base_commit: base_commit.clone(),
                            spawned,
                        };
                        launch_runner(shared.clone(), input);
                        infos.push(info_of(CandidateStatus::Working, Some(pid)));
                    }
                    Err(e) => {
                        finish_failed(shared, candidate_id, format!("agent spawn failed: {e}"));
                        infos.push(info_of(CandidateStatus::Failed, None));
                    }
                }
            }
            Err(e) => {
                finish_failed(
                    shared,
                    candidate_id,
                    format!("worktree provisioning failed: {e}"),
                );
                infos.push(info_of(CandidateStatus::Failed, None));
            }
        }
    }

    let _ = db_store(shared, |store| {
        store.update_task_status(&task_id, TaskStatus::Running)
    });
    // Every variant may have failed synchronously (bad refs, spawn errors);
    // this transitions the task to EVALUATING when nothing is left alive.
    maybe_close_task(shared, &task_id);

    Ok(SpawnSwarmInfo {
        task_id,
        base_commit,
        base_branch,
        candidates: infos,
    })
}

// ---------------------------------------------------------------------------
// Candidate runner.
// ---------------------------------------------------------------------------

/// How the poll loop ended; each variant maps to its own grading path.
enum RunnerExit {
    /// The agent left on its own (status already reaped by the poll).
    Natural(ExitStatus),
    /// `KillCandidate` RPC or daemon shutdown requested termination.
    Signalled(String),
    /// SPEC §8 idle watchdog: runtime exceeded the budget without a new commit.
    IdleTimeout,
    /// The supervisor could not poll the child reliably.
    Watchdog(String),
}

/// Register the kill channel and detach the runner thread.
fn launch_runner(shared: Arc<DaemonShared>, input: RunnerInput) {
    let (kill_tx, kill_rx) = channel::<()>();
    shared
        .live
        .lock()
        .expect("live map poisoned")
        .insert(input.candidate_id.clone(), kill_tx);
    let candidate_id = input.candidate_id.clone();
    if thread::Builder::new()
        .name(format!("gt-agent-{}", short_id(&candidate_id)))
        .spawn(move || run_candidate(shared, input, kill_rx))
        .is_err()
    {
        eprintln!("[GhostTree] failed to spawn runner thread for {candidate_id}");
    }
}

/// Own one agent process from spawn to terminal state (SPEC §3.2).
fn run_candidate(shared: Arc<DaemonShared>, input: RunnerInput, kill_rx: Receiver<()>) {
    let RunnerInput {
        task_id,
        candidate_id,
        worktree,
        test_cmd,
        timeout,
        base_commit,
        mut spawned,
    } = input;

    let mut last_head: Option<String> = None;
    let mut last_progress = Instant::now();
    let mut next_commit_check = Instant::now() + COMMIT_POLL;

    let exit = loop {
        if shared.shutdown.load(std::sync::atomic::Ordering::SeqCst) {
            break RunnerExit::Signalled("daemon shutdown".to_string());
        }
        match spawned.wait_timeout(STATUS_POLL) {
            Ok(Some(status)) => break RunnerExit::Natural(status),
            Ok(None) => {}
            Err(e) => break RunnerExit::Watchdog(format!("supervisor poll failed: {e}")),
        }
        if kill_rx.try_recv().is_ok() {
            break RunnerExit::Signalled("killed via KillCandidate RPC".to_string());
        }
        if last_progress.elapsed() >= timeout {
            break RunnerExit::IdleTimeout;
        }
        // SPEC §6.2/§8: agent commits are progress. Re-reading the worktree
        // HEAD on cadence gives an idle clock that resets when the branch
        // moves; the sha lands in the DB for `GetStatus` and as a GIT_COMMIT
        // event.
        if Instant::now() >= next_commit_check {
            next_commit_check = Instant::now() + COMMIT_POLL;
            if let Ok(head) = Repo::worktree_head(&worktree) {
                if head != base_commit && last_head.as_deref() != Some(head.as_str()) {
                    last_head = Some(head.clone());
                    last_progress = Instant::now();
                    let _ = db_store(&shared, |store| {
                        store.set_candidate_head(&candidate_id, &head)?;
                        store.append_event(&candidate_id, EventType::GitCommit, &head)
                    });
                }
            }
        }
    };

    // WORKING -> TESTING (SPEC §3.2): the agent left; grading decides the rest.
    let _ = db_store(&shared, |store| {
        store.set_candidate_status(&candidate_id, CandidateStatus::Testing)
    });
    notify_state(
        &shared,
        &candidate_id,
        CandidateStatus::Testing,
        last_head.as_deref(),
    );

    // Reap/escalate the process group and settle the exit bookkeeping.
    let (exit_code, stop_reason): (Option<i32>, Option<String>) = match &exit {
        RunnerExit::Natural(status) => {
            if let Err(e) = spawned.reap(*status) {
                eprintln!(
                    "[GhostTree] candidate {} log pump close error: {e}",
                    short_id(&candidate_id)
                );
            }
            (status.code(), None)
        }
        RunnerExit::Signalled(reason) => match spawned.terminate(TERMINATION_GRACE) {
            Ok(outcome) => (outcome.status.code(), Some(reason.clone())),
            Err(e) => (None, Some(format!("{reason} (kill failed: {e})"))),
        },
        RunnerExit::IdleTimeout => match spawned.terminate(TERMINATION_GRACE) {
            Ok(_) => (
                None,
                Some(format!(
                    "idle for {timeout:?} without a new commit (SPEC §8 timeout)"
                )),
            ),
            Err(e) => (None, Some(format!("idle timeout kill failed: {e}"))),
        },
        RunnerExit::Watchdog(message) => match spawned.terminate(TERMINATION_GRACE) {
            Ok(_) => (None, Some(message.clone())),
            Err(e) => (None, Some(format!("{message} (kill failed: {e})"))),
        },
    };

    // Grading: a natural agent exit may enter the test harness; kill/timeout
    // paths fail outright. The arbitrator persists parsed test counters and
    // diff statistics so scoring has real inputs (SPEC §7).
    let ready = if let RunnerExit::Natural(status) = &exit {
        if !status.success() {
            false
        } else {
            match &test_cmd {
                Some(cmd) => match run_candidate_tests(
                    &shared,
                    &task_id,
                    &candidate_id,
                    &worktree,
                    cmd,
                    &base_commit,
                ) {
                    Ok(ready) => ready,
                    Err((_, message)) => {
                        let _ = db_store(&shared, |store| {
                            store.append_event(&candidate_id, EventType::TestRun, &message)
                        });
                        false
                    }
                },
                None => true,
            }
        }
    } else {
        false
    };

    let final_status = if ready {
        CandidateStatus::Ready
    } else {
        CandidateStatus::Failed
    };
    let _ = db_store(&shared, |store| {
        if let Some(reason) = &stop_reason {
            store.append_event(&candidate_id, EventType::Syscall, reason)?;
        }
        store.finish_candidate(&candidate_id, exit_code)?;
        store.set_candidate_status(&candidate_id, final_status)
    });
    notify_state(&shared, &candidate_id, final_status, last_head.as_deref());

    shared
        .live
        .lock()
        .expect("live map poisoned")
        .remove(&candidate_id);
    maybe_close_task(&shared, &task_id);
}

/// Grade one candidate's test phase (SPEC §7 inputs): run the harness with a
/// hard timeout, persist parsed `test_results_json` counters, committed
/// `diff_stat_json`, and a TEST_RUN event; report whether the phase passed.
fn run_candidate_tests(
    shared: &Arc<DaemonShared>,
    task_id: &str,
    candidate_id: &str,
    worktree: &Path,
    test_cmd: &str,
    base_commit: &str,
) -> StateResult<bool> {
    let outcome = arbitrator::run_test_cmd(
        worktree,
        test_cmd,
        &test_log_path(shared, task_id, candidate_id),
    )
    .map_err(server_error)?;

    let log_text = std::fs::read_to_string(&outcome.log).unwrap_or_default();
    let results = arbitrator::results_from_output(
        arbitrator::parse_test_output(&log_text),
        outcome.success,
        outcome.duration.as_millis() as u64,
    );

    let diff_stat = arbitrator::compute_diff_stat(worktree, base_commit);

    db_store(shared, |store| {
        store.set_candidate_test_results(candidate_id, &results)?;
        if let Ok(Some(stat)) = &diff_stat {
            store.set_candidate_diff_stat(candidate_id, stat)?;
        }
        let payload = json!({
            "cmd": test_cmd,
            "exit_code": outcome.exit_code,
            "timed_out": outcome.timed_out,
            "duration_ms": outcome.duration.as_millis() as u64,
            "log": outcome.log.display().to_string(),
        })
        .to_string();
        store.append_event(candidate_id, EventType::TestRun, &payload)
    })?;

    if let Err(message) = diff_stat {
        eprintln!("[GhostTree] candidate {candidate_id} diff stat unavailable: {message}");
    }

    Ok(outcome.success)
}

// ---------------------------------------------------------------------------
// KillCandidate (SPEC §6.2) + orphan handling.
// ---------------------------------------------------------------------------

/// `KillCandidate` RPC body. `Ok(true)` — the live runner got the kill
/// signal (it owns the SIGTERM ladder); `Ok(false)` — the candidate was an
/// orphan whose process group was terminated directly by this call.
pub(crate) fn kill_candidate(shared: &Arc<DaemonShared>, candidate_id: &str) -> StateResult<bool> {
    let (pid, active) = {
        let store = store_lock(shared)?;
        let candidate = store
            .get_candidate(candidate_id)
            .map_err(db_to_state)?
            .ok_or_else(|| bad_params(format!("unknown candidate {candidate_id:?}")))?;
        (
            candidate.pid,
            candidate_is_active(candidate.status.as_ref()),
        )
    };

    // Fast path: the runner owns the handle and its full lifecycle.
    if let Some(kill) = shared
        .live
        .lock()
        .expect("live map poisoned")
        .get(candidate_id)
        .cloned()
    {
        if kill.send(()).is_ok() {
            return Ok(true);
        }
        // The runner just vanished (e.g. shutdown race); fall through and
        // terminate the group ourselves.
    }

    if !active {
        return Err(bad_params(format!(
            "candidate {candidate_id:?} is not running (nothing to kill)"
        )));
    }

    match pid32(pid) {
        Some(pgid) => Ok(terminate_orphan_group(pgid)),
        None if pid.is_none() => {
            // Active in the DB but never spawned to completion (provisioning
            // died between the row insert and the agent launch).
            finish_failed(
                shared,
                candidate_id,
                "killed while INITIALIZING (no live process)".to_string(),
            );
            Ok(false)
        }
        None => Ok(false),
    }
}

/// Narrow a stored pid to the libc pid domain (`None` = corrupt row).
fn pid32(pid: Option<i64>) -> Option<i32> {
    pid.and_then(|pid| {
        u32::try_from(pid)
            .ok()
            .filter(|p| *p <= i32::MAX as u32)
            .map(|p| p as i32)
    })
}

/// Best-effort stop for a candidate group whose runner thread is gone
/// (survived daemon death): SIGTERM the group, bounded wait, SIGKILL.
/// True if the group is not running once the ladder completes.
fn terminate_orphan_group(pgid: i32) -> bool {
    signal_group(pgid, libc::SIGTERM);
    let deadline = Instant::now() + ORPHAN_KILL_GRACE;
    while Instant::now() < deadline {
        if !group_alive(pgid) {
            return true;
        }
        thread::sleep(Duration::from_millis(20));
    }
    signal_group(pgid, libc::SIGKILL);
    let deadline = Instant::now() + Duration::from_millis(500);
    while Instant::now() < deadline && group_alive(pgid) {
        thread::sleep(Duration::from_millis(20));
    }
    group_alive(pgid)
}

/// Signal a whole process group (negative pid == pgid).
fn signal_group(pgid: i32, sig: i32) {
    // SAFETY: pure killpg wrapper.
    unsafe {
        libc::kill(-pgid, sig);
    }
}

/// Group exists for the current user? (EPERM also means "alive".)
fn group_alive(pgid: i32) -> bool {
    // SAFETY: probe-only signal (0).
    let rc = unsafe { libc::kill(-pgid, 0) };
    rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

// ---------------------------------------------------------------------------
// Boot reconciliation (SPEC §8).
// ---------------------------------------------------------------------------

/// SPEC §8: prune orphan worktree administrative files, stop
/// active-but-unowned candidate groups, mark their rows FAILED, and move
/// tasks that lost their last active candidate to ABORTED. Returns how many
/// candidates were recovered.
pub(crate) fn reconcile(shared: &Arc<DaemonShared>) -> Result<usize, String> {
    let repo = Repo::discover(&shared.root).map_err(|e| e.to_string())?;
    repo.worktree_prune().map_err(|e| e.to_string())?;

    let mut recovered = 0usize;
    let mut affected_tasks: Vec<String> = Vec::new();

    {
        let store = store_lock(shared).map_err(|(_, msg)| msg)?;
        for candidate in store.list_all_candidates().map_err(|e| e.to_string())? {
            if !candidate_is_active(candidate.status.as_ref()) {
                continue;
            }
            let stop_reason = {
                let wt_abs = shared.root.join(&candidate.worktree_path);
                let worktree_present = fs::metadata(&wt_abs).map(|m| m.is_dir()).unwrap_or(false);
                if !worktree_present {
                    format!("worktree {} is gone", candidate.worktree_path)
                } else {
                    match pid32(candidate.pid) {
                        Some(pgid) => {
                            if terminate_orphan_group(pgid) {
                                format!(
                                    "unowned agent group (pid {}) terminated at boot",
                                    candidate.pid.unwrap_or(-1)
                                )
                            } else {
                                format!(
                                    "unowned agent group (pid {}) could not be stopped",
                                    candidate.pid.unwrap_or(-1)
                                )
                            }
                        }
                        None => "candidate never started (no pid recorded)".to_string(),
                    }
                }
            };
            let payload = format!("boot recovery: {stop_reason}");
            store
                .append_event(&candidate.id, EventType::Syscall, &payload)
                .map_err(|e| e.to_string())?;
            store
                .finish_candidate(&candidate.id, None)
                .map_err(|e| e.to_string())?;
            store
                .set_candidate_status(&candidate.id, CandidateStatus::Failed)
                .map_err(|e| e.to_string())?;
            recovered += 1;
            if !affected_tasks.contains(&candidate.task_id) {
                affected_tasks.push(candidate.task_id.clone());
            }
        }

        // Tasks whose last active candidate vanished: SPEC §8 wording is
        // "mark unclosed [runs] as ABORTED" — at task level that is ABORTED.
        for task_id in &affected_tasks {
            if let Some(task) = store.get_task(task_id).map_err(|e| e.to_string())? {
                if matches!(
                    task.status.as_ref(),
                    Ok(TaskStatus::Pending | TaskStatus::Running)
                ) {
                    let candidates = store.list_candidates(task_id).map_err(|e| e.to_string())?;
                    if candidates
                        .iter()
                        .all(|c| !candidate_is_active(c.status.as_ref()))
                    {
                        store
                            .update_task_status(task_id, TaskStatus::Aborted)
                            .map_err(|e| e.to_string())?;
                    }
                }
            }
        }
    }
    Ok(recovered)
}
