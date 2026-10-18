//! `gt init` (SPEC §5.1): provisions `.ghosttree/` inside the primary
//! repository — state database, logs, worktrees, default config, and a
//! `.gitignore` entry.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::db::Store;
use crate::git::Repo;

/// Default `.ghosttree/config.toml` written on init.
///
/// Commented keys document the built-in defaults; later phases extend this
/// file (daemon settings, scoring weights, per-agent command overrides).
const CONFIG_TEMPLATE: &str = r"
# GhostTree repository configuration (SPEC §2.1).
# Uncommented keys override the built-in defaults shown here.

[swarm]
# Maximum runtime of a single candidate agent, in seconds (SPEC §8).
# candidate_timeout_secs = 900
# Command used by the arbitrator to score candidates (SPEC §5.1 gt swarm).
# test_cmd = 'cargo test'
";

/// Directory names below `.ghosttree/` (SPEC §2.1 file system hierarchy).
const GHOST_DIR: &str = ".ghosttree";

pub fn run(force: bool) -> Result<()> {
    let repo = Repo::discover(Path::new("."))
        .context("gt init must run inside a git repository (run `git init` first)")?;
    let root: PathBuf = repo.root().to_path_buf();

    // Verify worktree plumbing works in this repository (SPEC §4.1 step 2).
    repo.worktree_list()
        .context("git worktree plumbing is unavailable in this repository")?;

    let ghost_dir = root.join(GHOST_DIR);
    fs::create_dir_all(ghost_dir.join("logs"))
        .with_context(|| format!("failed to create {}", ghost_dir.join("logs").display()))?;
    fs::create_dir_all(ghost_dir.join("worktrees"))
        .with_context(|| format!("failed to create {}", ghost_dir.join("worktrees").display()))?;

    // Build (or open) the WAL state store; this applies the §3.1 schema.
    let db_path = ghost_dir.join("ghosttree.db");
    Store::open(&db_path).with_context(|| format!("failed to initialize {}", db_path.display()))?;

    // Default config: never clobber customizations without --force.
    let config_path = ghost_dir.join("config.toml");
    let config_existed = config_path.exists();
    let config_action = if force || !config_existed {
        fs::write(&config_path, CONFIG_TEMPLATE)
            .with_context(|| format!("failed to write {}", config_path.display()))?;
        if config_existed {
            "rewritten (--force)"
        } else {
            "written"
        }
    } else {
        "kept (exists; use --force to overwrite)"
    };

    // Ensure `.ghosttree/` is ignored by git.
    let gitignore_path = root.join(".gitignore");
    let mut content = fs::read_to_string(&gitignore_path).unwrap_or_default();
    let already_ignored = content.lines().any(|line| {
        let line = line.trim();
        line == GHOST_DIR || line == ".ghosttree"
    });
    let gitignore_action = if already_ignored {
        "already ignored"
    } else {
        if !content.is_empty() && !content.ends_with('\n') {
            content.push('\n');
        }
        content.push_str("# Added by GhostTree\n.ghosttree/\n");
        fs::write(&gitignore_path, content)
            .with_context(|| format!("failed to update {}", gitignore_path.display()))?;
        "added .ghosttree/"
    };

    println!("Initialized GhostTree at {}", root.display());
    println!("  state db:   {GHOST_DIR}/ghosttree.db");
    println!("  logs:       {GHOST_DIR}/logs/");
    println!("  worktrees:  {GHOST_DIR}/worktrees/");
    println!("  config:     {GHOST_DIR}/config.toml ({config_action})");
    println!("  .gitignore: {gitignore_action}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_template_is_valid_toml() {
        let parsed: toml::Value = toml::from_str(CONFIG_TEMPLATE).unwrap();
        assert!(parsed.get("swarm").is_some());
    }
}
