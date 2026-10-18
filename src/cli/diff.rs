//! `gt diff <candidate-id>` (SPEC §5.1): unified patch of the candidate's
//! committed work against its task base commit, or `--stat` counters.

use anyhow::{bail, Context, Result};

use crate::cli::{open_store, resolve_candidate};

pub(crate) fn run(candidate_ref: String, stat: bool) -> Result<()> {
    let (repo, store) = open_store()?;
    let candidate_id = resolve_candidate(&store, &candidate_ref)?;
    let candidate = store
        .get_candidate(&candidate_id)
        .context("state store read")?
        .with_context(|| format!("candidate {candidate_id:?} vanished during lookup"))?;

    let task = store
        .get_task(&candidate.task_id)
        .context("state store read")?
        .with_context(|| format!("task {} missing for candidate", candidate.task_id))?;

    // Prefer the recorded head; fall back to the candidate branch so
    // history survives worktree removal.
    let to = candidate
        .head_commit
        .clone()
        .unwrap_or_else(|| candidate.branch_name.clone());

    if stat {
        let stat =
            crate::arbitrator::diff_stat_from_numstat(repo.diff_numstat(&task.base_commit, &to)?);
        let stat_json = serde_json::to_string_pretty(&stat).context("numstat rendering")?;
        println!("{stat_json}");
        return Ok(());
    }

    let patch = repo.diff_patch(&task.base_commit, &to)?;
    if patch.trim().is_empty() {
        bail!(
            "candidate has no committed changes versus base {}. \
             agent log: .ghosttree/logs/{}/{}/{}.raw.log",
            task.base_commit,
            task.id,
            candidate.id,
            candidate.id
        );
    }
    println!("{patch}");
    Ok(())
}
