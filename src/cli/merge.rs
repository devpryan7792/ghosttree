//! `gt merge <candidate-id>` (SPEC §4.3): promote a candidate branch into
//! the active branch with a merge-tree dry run, dirty-index protection
//! (SPEC §8), and MERGED/COMPLETED bookkeeping.

use anyhow::{bail, Context, Result};

use crate::cli::{open_store, resolve_candidate};
use crate::db::{short_id, CandidateStatus, TaskStatus};

pub(crate) fn run(candidate_ref: String, squash: bool, autostash: bool) -> Result<()> {
    let (repo, store) = open_store()?;
    let candidate_id = resolve_candidate(&store, &candidate_ref)?;
    let candidate = store
        .get_candidate(&candidate_id)
        .context("state store read")?
        .with_context(|| format!("candidate {candidate_id:?} vanished during lookup"))?;

    // Lifecycle guard (SPEC §3.2): only settled, unmerged candidates carry
    // promotable work.
    // Lifecycle guard (SPEC §3.2): only settled, unmerged candidates carry
    // promotable work.
    match candidate.status.as_ref() {
        Ok(CandidateStatus::Ready) => {}
        Ok(other) => bail!(
            "candidate {} is {}; only READY candidates can be merged",
            candidate.id,
            other.as_str()
        ),
        Err(e) => bail!("candidate status corrupted: {e}"),
    }

    let task = store
        .get_task(&candidate.task_id)
        .context("state store read")?
        .with_context(|| format!("task {} missing", candidate.task_id))?;

    let head = candidate
        .head_commit
        .clone()
        .unwrap_or_else(|| candidate.branch_name.clone());

    // Dry run first: never leave the primary repo with a conflicted index.
    let merged_head = repo.rev_parse("HEAD").context("primary HEAD lookup")?;
    let dry_run = repo
        .merge_tree(&task.base_commit, &merged_head, &head)
        .context("merge dry run")?;
    if dry_run.conflicted {
        bail!(
            "merge of {} conflicts with the active branch; conflicts:\n  {}",
            candidate.branch_name,
            dry_run.conflict_output.join("\n  ")
        );
    }
    if repo.is_dirty().context("dirty-index check")? {
        if !autostash {
            bail!(
                "{} has uncommitted changes; commit them, or pass --autostash \
                 to snapshot the state with `git stash create` first (SPEC §8)",
                repo.root().display()
            );
        }
        let stash = repo
            .stash_autostash(&format!(
                "ghosttree: pre-merge autostash (candidate {}, task {})",
                short_id(&candidate.id),
                short_id(&task.id)
            ))
            .context("autostash")?;
        println!("[GhostTree] stashed dirty state as {stash} (recover: git stash apply {stash})");
    }

    let message = if squash {
        format!(
            "ghosttree(task): squash merge variant {} for task {}",
            candidate.variant_name, task.id
        )
    } else {
        format!(
            "ghosttree(task): merge variant {} for task {}",
            candidate.variant_name, task.id
        )
    };

    if squash {
        repo.merge_squash(&candidate.branch_name)
            .context("squash merge")?;
        // `merge --squash` stages without committing; identical trees stage
        // nothing and a commit would fail, so detect that explicitly.
        if repo.staged_changes().context("staged diff check")? {
            repo.commit_index(&message).context("squash commit")?;
        } else {
            println!(
                "[GhostTree] nothing to squash: {} tree is identical to HEAD",
                candidate.variant_name
            );
        }
    } else {
        repo.merge_no_ff(&candidate.branch_name, &message)
            .context("merge")?;
    }

    let new_head = repo.rev_parse("HEAD").context("post-merge HEAD lookup")?;

    // Bookkeeping: MERGED candidate, winner fill-in, terminal task status.
    store
        .set_candidate_status(&candidate.id, CandidateStatus::Merged)
        .context("state store update")?;
    if task.winner_candidate_id.is_none() {
        store
            .set_task_winner(&task.id, &candidate.id)
            .context("state store update")?;
    }
    // A merge settles the task (SPEC §3.2: promotion completes the run).
    if matches!(
        task.status.as_ref(),
        Ok(TaskStatus::Pending | TaskStatus::Running | TaskStatus::Evaluating)
    ) {
        store
            .update_task_status(&task.id, TaskStatus::Completed)
            .context("state store update")?;
    }

    println!(
        "[GhostTree] Merged {} ({}) into {} at {}{}",
        candidate.variant_name,
        short_id(&candidate.id),
        repo.current_branch()
            .context("branch lookup")?
            .unwrap_or_else(|| "(detached HEAD)".to_string()),
        new_head.chars().take(7).collect::<String>(),
        if squash { " (squashed)" } else { "" },
    );
    Ok(())
}
