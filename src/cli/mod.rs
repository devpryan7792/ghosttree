//! CLI surface (SPEC §5) — parser plus command dispatch.

mod daemon;
mod diff;
mod eval;
mod init;
mod list;
mod merge;
mod purge;
mod swarm;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use std::path::Path;
use std::process::ExitCode;

#[derive(Debug, Parser)]
#[command(
    name = "gt",
    bin_name = "gt",
    version,
    about = "GhostTree: autonomous Git worktree swarm orchestrator"
)]
pub struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
pub enum Commands {
    /// Initialize .ghosttree state in the current git repository.
    Init {
        /// Overwrite .ghosttree/config.toml with the default template.
        #[arg(long)]
        force: bool,
    },
    /// List tasks and their candidates.
    List {
        /// Emit machine-readable JSON instead of a text table.
        #[arg(long)]
        json: bool,
        /// Include terminal tasks (COMPLETED / ABORTED).
        #[arg(long)]
        all: bool,
    },
    /// Control the background supervisor daemon (SPEC §5.1).
    Daemon {
        #[command(subcommand)]
        action: DaemonAction,
    },
    /// Spawn N competing agent variants on a prompt and watch them live.
    Swarm {
        /// Task prompt handed to every agent variant.
        #[arg()]
        prompt: String,
        #[arg(short = 'n', long = "variants")]
        /// Number of competing variants to spawn.
        variants: usize,
        /// Comma-separated agent commands, one per variant
        /// (e.g. "claude-code --run, aider --yes").
        #[arg(long = "agents")]
        agents: String,
        /// Test command the arbitrator runs in each worktree after its
        /// agent exits (SPEC §7 grading input).
        #[arg(long = "test-cmd")]
        test_cmd: Option<String>,
        /// Per-candidate idle timeout in seconds (SPEC §8, default 900).
        #[arg(long)]
        timeout_secs: Option<u64>,
        /// Stream transitions as plain lines instead of the live dashboard.
        #[arg(long)]
        no_ui: bool,
    },
    /// Show a candidate's committed diff against its task base.
    Diff {
        /// Candidate id (full, 7-char prefix, or variant name).
        #[arg()]
        candidate_id: String,
        /// Summary counters instead of the unified patch.
        #[arg(long)]
        stat: bool,
    },
    /// Run the arbitrator over a task's settled candidates (SPEC §7).
    Eval {
        #[arg()]
        task_id: String,
        /// Execute the task's test command again in READY worktrees before
        /// scoring (SPEC §5.1 `--re-run`).
        #[arg(long = "re-run")]
        re_run: bool,
        /// Override the recovered/guessed test command used with --re-run.
        #[arg(long = "test-cmd")]
        test_cmd: Option<String>,
    },
    /// Promote a candidate's branch into the active branch (SPEC §4.3).
    Merge {
        /// Candidate id (full, 7-char prefix, or variant name).
        #[arg()]
        candidate_id: String,
        /// Squash the candidate's commits into one new commit.
        #[arg(long)]
        squash: bool,
        /// Stash the primary repo's dirty state with `git stash create`
        /// before merging instead of refusing (SPEC §8).
        #[arg(long)]
        autostash: bool,
    },
    /// Remove worktrees, delete candidate branches, and drop task rows.
    Purge {
        /// Task id (full or 7-char prefix); omit with --all.
        #[arg()]
        task_id: Option<String>,
        /// Purge every task.
        #[arg(long)]
        all: bool,
        /// Print the removal plan without touching anything.
        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(Debug, Subcommand)]
pub enum DaemonAction {
    /// Start the daemon in the background (logs to .ghosttree/logs/daemon.log).
    ///
    /// With `--foreground`, run against the terminal instead: SIGTERM/SIGINT
    /// stop it cleanly.
    Start {
        /// Run in the foreground instead of daemonizing.
        #[arg(long)]
        foreground: bool,
        /// Also accept RPCs on 127.0.0.1:<port> (the UDS is always primary).
        #[arg(long)]
        port: Option<u16>,
    },
    /// Send SIGTERM to the daemon and wait for its clean shutdown.
    ///
    /// `--force` escalates to SIGKILL after the grace window and removes
    /// any leftover socket/pid files.
    Stop {
        /// Kill after the grace window instead of erroring out.
        #[arg(long)]
        force: bool,
    },
}

/// Parse argv, dispatch the command, and translate errors into an exit code.
pub fn run() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.command {
        Commands::Init { force } => init::run(force),
        Commands::List { json, all } => list::run(json, all),
        Commands::Daemon { action } => crate::cli::daemon::dispatch(action),
        Commands::Swarm {
            prompt,
            variants,
            agents,
            test_cmd,
            timeout_secs,
            no_ui,
        } => swarm::run(prompt, variants, agents, test_cmd, timeout_secs, no_ui),
        Commands::Diff { candidate_id, stat } => diff::run(candidate_id, stat),
        Commands::Eval {
            task_id,
            re_run,
            test_cmd,
        } => eval::run(task_id, re_run, test_cmd),
        Commands::Merge {
            candidate_id,
            squash,
            autostash,
        } => merge::run(candidate_id, squash, autostash),
        Commands::Purge {
            task_id,
            all,
            dry_run,
        } => purge::run(task_id, all, dry_run),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {err:#}");
            ExitCode::FAILURE
        }
    }
}

/// Discover the repo and open the state store (stateful subcommands).
pub(crate) fn open_store() -> Result<(crate::git::Repo, crate::db::Store)> {
    let repo = crate::git::Repo::discover(Path::new("."))
        .context("this command must run inside a git repository")?;
    let db_path = repo.root().join(".ghosttree").join("ghosttree.db");
    if !db_path.exists() {
        bail!(
            "GhostTree is not initialized here (missing {}); run `gt init` first",
            db_path.display()
        );
    }
    let store = crate::db::Store::open(&db_path)
        .with_context(|| format!("failed to open {}", db_path.display()))?;
    Ok((repo, store))
}

/// Resolve a user-supplied candidate reference into the full db id:
/// exact id, unique 7-char id prefix, or unique variant name.
pub(crate) fn resolve_candidate(store: &crate::db::Store, input: &str) -> Result<String> {
    if store.get_candidate(input)?.is_some() {
        return Ok(input.to_string());
    }
    let all = store.list_all_candidates()?;
    let prefix_matches: Vec<&crate::db::Candidate> = all
        .iter()
        .filter(|c| !input.is_empty() && c.id.starts_with(input))
        .collect();
    let variant_matches: Vec<&crate::db::Candidate> =
        all.iter().filter(|c| c.variant_name == input).collect();
    match (prefix_matches.as_slice(), variant_matches.as_slice()) {
        ([candidate], _) | (_, [candidate]) => Ok(candidate.id.clone()),
        ([_, ..], _) => bail!(
            "candidate id prefix {input:?} matches {} candidates; be more specific",
            prefix_matches.len()
        ),
        (_, [_, ..]) => bail!(
            "variant name {input:?} is ambiguous across tasks {}; use a longer candidate id prefix",
            variant_matches
                .iter()
                .map(|c| crate::db::short_id(&c.task_id))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        (_, _) => bail!("unknown candidate {input:?}"),
    }
}
