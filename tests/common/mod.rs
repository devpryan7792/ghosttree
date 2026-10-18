//! Shared test fixtures: hermetic temporary git repositories.
//!
//! Global git config is disabled (`GIT_CONFIG_GLOBAL/SYSTEM=/dev/null`) so
//! every commit must pass explicit `-c user.*` flags; that keeps CI machines
//! without a configured identity working.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use tempfile::{tempdir, TempDir};

pub struct TestRepo {
    _dir: TempDir,
    pub root: PathBuf,
}

/// Run a git command in `root`, asserting success; returns trimmed stdout.
pub fn git(root: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("failed to spawn git");
    assert!(
        out.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim_end().to_string()
}

/// Commit helper: writes a file, stages it, commits, returns the new HEAD sha.
pub fn commit_file(root: &Path, name: &str, content: &str, message: &str) -> String {
    fs::write(root.join(name), content).expect("write fixture file");
    git(root, &["add", name]);
    git(
        root,
        &[
            "-c",
            "user.name=GT Test",
            "-c",
            "user.email=gt@test",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-m",
            message,
        ],
    );
    git(root, &["rev-parse", "HEAD"])
}

impl TestRepo {
    /// Create a temporary repository with one initial commit on `main`.
    pub fn new() -> Self {
        let dir = tempdir().expect("tempdir");
        let root = dir.path().to_path_buf();
        git(&root, &["init", "-b", "main"]);
        commit_file(&root, "README.md", "ghosttree test repo\n", "initial");
        commit_file(&root, "src.txt", "line1\nline2\nline3\n", "add src");
        Self { _dir: dir, root }
    }
}
