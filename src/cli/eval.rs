//! `gt eval <task-id>` (SPEC §5.1): run the arbitrator across a task's
//! settled candidates, scoring them (SPEC §7) and persisting the winner.
//!
//! `--re-run` first re-executes the task's test command in every READY
//! worktree so a flaky candidate can earn (or lose) its promotion.

use anyhow::{bail, Context, Result};

use crate::arbitrator;
use crate::cli::open_store;
use crate::db::{short_id, CandidateStatus, EventType, Store};

pub(crate) fn run(task_ref: String, re_run: bool, test_cmd: Option<String>) -> Result<()> {
    let (repo, store) = open_store()?;
    let task_id = resolve_task(&store, &task_ref)?;
    let task = store
        .get_task(&task_id)
        .context("state store read")?
        .with_context(|| format!("unknown task {task_ref:?}"))?;

    if re_run {
        rerun_ready_tests(&repo, &store, &task_id, test_cmd)?;
    } else if test_cmd.is_some() {
        bail!("--test-cmd is only used together with --re-run");
    }

    let evaluation = arbitrator::evaluate_task(&store, &repo, &task_id)
        .map_err(|e| anyhow::anyhow!("evaluation failed: {e}"))?;

    println!(
        "[GhostTree] Task {} — {} ({} candidates settled)",
        short_id(&task_id),
        task.title,
        evaluation.scored.len()
    );
    for scored in &evaluation.scored {
        let b = &scored.breakdown;
        println!(
            "  {:<7} {:<12} {:<9} score {:>6.1}  (tests {:.0}, cov {:.0}, churn {:.1}, time {:.2})",
            short_id(&scored.candidate_id),
            scored.variant_name,
            scored.status,
            scored.score,
            b.s_tests,
            b.s_cov,
            b.s_churn,
            b.s_time,
        );
    }
    match &evaluation.winner_candidate_id {
        Some(winner) => {
            let scored = evaluation
                .scored
                .iter()
                .find(|s| &s.candidate_id == winner)
                .expect("winner present in scored list");
            println!(
                "[GhostTree] Winner: {} ({}, score {:.1}); task COMPLETED.",
                short_id(winner),
                scored.variant_name,
                scored.score
            );
            println!(
                "Run 'gt merge {}' to promote changes into the active branch.",
                short_id(winner)
            );
        }
        None => println!("[GhostTree] No promotable candidate; task stays in EVALUATING."),
    }
    Ok(())
}

/// Accept full ids and unique 7-char prefixes for tasks.
fn resolve_task(store: &Store, input: &str) -> Result<String> {
    if store.get_task(input)?.is_some() {
        return Ok(input.to_string());
    }
    let prefix: Vec<String> = store
        .list_tasks(true)?
        .into_iter()
        .filter(|t| t.id.starts_with(input))
        .map(|t| t.id)
        .collect();
    match prefix.len() {
        1 => Ok(prefix[0].clone()),
        0 => bail!("unknown task {input:?}"),
        _ => bail!(
            "task id prefix {input:?} is ambiguous ({} matches)",
            prefix.len()
        ),
    }
}

/// Execute the test command again in each READY candidate's worktree and
/// persist the fresh results. FAILED candidates are out of scope: the
/// arbitrator promotion path only considers READY runs.
fn rerun_ready_tests(
    repo: &crate::git::Repo,
    store: &Store,
    task_id: &str,
    override_cmd: Option<String>,
) -> Result<()> {
    let test_cmd = match override_cmd {
        Some(cmd) => Some(cmd),
        None => arbitrator::recover_test_cmd(store, task_id),
    }
    .with_context(|| {
        "no test command available for --re-run; pass --test-cmd or spawn the task with one"
    })?;

    for candidate in store.list_candidates(task_id)? {
        if !matches!(candidate.status.as_ref(), Ok(CandidateStatus::Ready)) {
            continue;
        }
        let worktree = repo.root().join(&candidate.worktree_path);
        if !worktree.is_dir() {
            println!(
                "  {} ({}) skipped: worktree {} is gone",
                short_id(&candidate.id),
                candidate.variant_name,
                candidate.worktree_path
            );
            continue;
        }
        let log = repo
            .root()
            .join(".ghosttree/logs")
            .join(task_id)
            .join(format!("{}.tests.log", candidate.id));
        let outcome = arbitrator::run_test_cmd(&worktree, &test_cmd, &log)
            .map_err(|e| anyhow::anyhow!("test rerun failed for {}: {e}", candidate.id))?;

        let results = arbitrator::results_from_output(
            arbitrator::parse_test_output(
                &std::fs::read_to_string(&outcome.log).unwrap_or_default(),
            ),
            outcome.success,
            outcome.duration.as_millis() as u64,
        );
        let status = if outcome.success {
            CandidateStatus::Ready
        } else {
            CandidateStatus::Failed
        };
        store
            .set_candidate_test_results(&candidate.id, &results)
            .context("persist test results")?;
        store
            .set_candidate_status(&candidate.id, status)
            .context("persist status")?;
        store
            .append_event(
                &candidate.id,
                EventType::TestRun,
                &serde_json::json!({
                    "cmd": test_cmd,
                    "exit_code": outcome.exit_code,
                    "timed_out": outcome.timed_out,
                    "duration_ms": outcome.duration.as_millis() as u64,
                    "log": log.display().to_string(),
                    "re_run": true,
                })
                .to_string(),
            )
            .context("persist test event")?;

        println!(
            "  {} ({}) re-run: {} (exit {:?}, log {})",
            short_id(&candidate.id),
            candidate.variant_name,
            if outcome.success { "PASS" } else { "FAIL" },
            outcome.exit_code,
            log.display()
        );
    }
    Ok(())
}
