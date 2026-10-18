//! `gt purge [task-id] [--all] [--dry-run]` (SPEC §5.1): remove ghost
//! worktrees, delete candidate branches, and drop task rows.
//!
//! Safety: a task with any active candidate (INITIALIZING/WORKING/TESTING)
//! is refused outright — work is possibly still supervised by the daemon.

use std::path::PathBuf;

use anyhow::{bail, Context, Result};

use crate::cli::open_store;
use crate::db::{short_id, CandidateStatus, Store};

pub(crate) fn run(task_ref: Option<String>, all: bool, dry_run: bool) -> Result<()> {
    match (&task_ref, all) {
        (Some(_), true) => bail!("pass a task id or --all, not both"),
        (None, false) => bail!("specify a task id or pass --all (see --dry-run to preview)"),
        _ => {}
    }

    let (repo, store) = open_store()?;
    let to_purge = resolve_tasks(&store, task_ref.as_deref(), all)?;
    if to_purge.is_empty() {
        bail!("no tasks to purge");
    }

    // Build the full plan before touching anything: an active candidate in
    // ANY purged task cancels the whole operation (work may still be live).
    let mut plans = Vec::new();
    for task in &to_purge {
        let candidates = store.list_candidates(&task.id)?;
        let active: Vec<String> = candidates
            .iter()
            .filter(|c| {
                matches!(
                    c.status.as_ref(),
                    Ok(CandidateStatus::Initializing
                        | CandidateStatus::Working
                        | CandidateStatus::Testing)
                )
            })
            .map(|c| {
                format!(
                    "  {} ({}), {}",
                    short_id(&c.id),
                    c.variant_name,
                    c.status.as_ref().map(|s| s.as_str()).unwrap_or("?")
                )
            })
            .collect();
        if !active.is_empty() {
            bail!(
                "task {} ({}): {} still active, refusing to purge\n{}\n\
                 stop them first (KillCandidate / gt daemon stop, or wait for settling)",
                short_id(&task.id),
                task.title,
                active.len(),
                active.join("\n")
            );
        }
        plans.push(PurgePlan {
            task_id: task.id.clone(),
            title: task.title.clone(),
            candidates: candidates
                .iter()
                .map(|c| CandidateCleanup {
                    candidate_id: c.id.clone(),
                    variant: c.variant_name.clone(),
                    worktree: repo.root().join(&c.worktree_path),
                    branch: c.branch_name.clone(),
                })
                .collect(),
        });
    }

    if dry_run {
        println!("[GhostTree] dry run — nothing will be removed:");
        print_plans(&plans);
        return Ok(());
    }

    for plan in &plans {
        for candidate in &plan.candidates {
            if candidate.worktree.is_dir() {
                if let Err(e) = repo.worktree_remove(
                    candidate
                        .worktree
                        .strip_prefix(repo.root())
                        .unwrap_or(&candidate.worktree),
                    true,
                ) {
                    eprintln!(
                        "[GhostTree] worktree {} removal: {e} (prune will clean admin files)",
                        candidate.worktree.display()
                    );
                }
            }
            if let Err(e) = repo.delete_branch(&candidate.branch) {
                eprintln!("[GhostTree] branch {}: {e}", candidate.branch);
            }
        }
        repo.worktree_prune().context("worktree prune")?;
        store
            .delete_task(&plan.task_id)
            .context("state store delete")?;
        println!(
            "[GhostTree] purged task {} ({}: {}): {} worktree(s), {} branch(es)",
            short_id(&plan.task_id),
            plan.title,
            plan.task_id,
            plan.candidates.len(),
            plan.candidates.len(),
        );
    }

    let mut out = repo.worktree_list().context("worktree listing")?;
    out.retain(|entry| entry.path != repo.root());
    println!(
        "[GhostTree] purge complete: {} leaked worktree(s) remain",
        out.len()
    );
    Ok(())
}

struct CandidateCleanup {
    candidate_id: String,
    variant: String,
    worktree: PathBuf,
    branch: String,
}

struct PurgePlan {
    task_id: String,
    title: String,
    candidates: Vec<CandidateCleanup>,
}

/// Resolve `--all` or a task id (full or unique 7-char prefix) into rows.
fn resolve_tasks(store: &Store, task_ref: Option<&str>, all: bool) -> Result<Vec<crate::db::Task>> {
    if all {
        return store.list_tasks(true).context("state store read");
    }
    let input = task_ref.expect("dispatch guarantees a task id or --all");
    if let Some(task) = store.get_task(input).context("state store read")? {
        return Ok(vec![task]);
    }
    let prefix: Vec<crate::db::Task> = store
        .list_tasks(true)?
        .into_iter()
        .filter(|t| t.id.starts_with(input))
        .collect();
    match prefix.len() {
        1 => Ok(prefix),
        0 => bail!("unknown task {input:?}"),
        _ => bail!(
            "task id prefix {input:?} is ambiguous ({} matches)",
            prefix.len()
        ),
    }
}

fn print_plans(plans: &[PurgePlan]) {
    for plan in plans {
        println!("  task {} `{}`:", short_id(&plan.task_id), plan.title);
        for candidate in &plan.candidates {
            println!(
                "    - worktree {} ({}) [candidate {}]",
                candidate.worktree.display(),
                candidate.variant,
                short_id(&candidate.candidate_id)
            );
            println!("      branch {} deleted", candidate.branch);
        }
        println!("    task row (and events) deleted");
    }
}
