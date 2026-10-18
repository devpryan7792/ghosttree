//! End-to-end swarm integration test (SPEC §10.1).
//!
//! Drives the real `gt` binary in a hermetic temp repo: `gt swarm` with
//! three competing mock agents (one correct fix, one high-churn
//! hallucination that fails tests, one that errors out), auto-started real
//! daemon, arbitrator scoring, `gt merge`, and `gt purge` with a
//! zero-leaked-worktrees assertion.

mod common;

use common::{git, TestRepo};

use anyhow::Result;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use assert_cmd::Command;
use predicates::str::contains;

/// Persistence for the fixture scripts (outside the repo, so they never
/// enter the base tree a worktree branches from).
struct Fixture {
    _dir: tempfile::TempDir,
    fixer: PathBuf,
    hallucinator: PathBuf,
    broken: PathBuf,
    test_cmd: PathBuf,
}

fn write_script(path: &Path, body: &str) {
    std::fs::write(path, body).expect("write fixture script");
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).expect("chmod fixture");
}

fn fixture() -> Result<Fixture> {
    let dir = tempfile::tempdir()?;
    let fixer = dir.path().join("fixer.sh");
    write_script(
        &fixer,
        r#"#!/bin/sh
echo "fixer agent: reproducing race condition"
printf 'line1\nline2\nline3\nfixed\n' > src.txt
printf '\n## fixed\nstress-tested\n' >> README.md
git add -A
git -c user.name=Fixer -c user.email=fixer@test -c commit.gpgsign=false commit -q -m "fix race condition in pool"
exit 0
"#,
    );
    let hallucinator = dir.path().join("hallucinator.sh");
    write_script(
        &hallucinator,
        r#"#!/bin/sh
echo "hallucinator agent: rewriting everything"
i=0
while [ $i -lt 12 ]; do
  yes "hallucinated implementation line" | head -40 > "module_$i.rs"
  i=$((i + 1))
done
rm -f README.md
git add -A
git -c user.name=Halla -c user.email=halla@test -c commit.gpgsign=false commit -q -m "bold rewrite of everything"
exit 0
"#,
    );
    let broken = dir.path().join("broken.sh");
    write_script(
        &broken,
        r#"#!/bin/sh
echo "broken agent: cannot reach autopilot" >&2
exit 3
"#,
    );
    let test_cmd = dir.path().join("run_tests.sh");
    write_script(
        &test_cmd,
        r#"#!/bin/sh
# Real fix: 'fixed' present in src.txt and no hallucinated modules.
if grep -q '^fixed$' src.txt 2>/dev/null && [ ! -e module_0.rs ]; then
    echo "test result: ok. 3 passed; 0 failed; 0 ignored; 0 measured"
    exit 0
fi
echo "test result: FAILED. 0 passed; 3 failed; 0 ignored; 0 measured"
exit 1
"#,
    );
    Ok(Fixture {
        _dir: dir,
        fixer,
        hallucinator,
        broken,
        test_cmd,
    })
}

/// `gt` runner rooted in the test repo. Git config is hermetic (global +
/// system config disabled) and the merge-commit identity comes from env
/// vars, mirroring what the daemon and agents receive.
fn gt(root: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_gt"));
    cmd.current_dir(root)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "GT E2E")
        .env("GIT_AUTHOR_EMAIL", "e2e@test")
        .env("GIT_COMMITTER_NAME", "GT E2E")
        .env("GIT_COMMITTER_EMAIL", "e2e@test");
    cmd
}

fn wait_gone(path: &Path, timeout: Duration, what: &str) {
    let deadline = Instant::now() + timeout;
    while path.exists() {
        assert!(
            Instant::now() < deadline,
            "{what} did not disappear within {timeout:?}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn swarm_picks_correct_fix_merges_and_purges_cleanly() -> Result<()> {
    let repo = TestRepo::new();
    let fixture = fixture()?;

    gt(&repo.root).arg("init").assert().success();
    // Commit the init-time .gitignore so the primary worktree the swarm
    // branches from is clean (mirrors committing infra in real projects).
    git(&repo.root, &["add", ".gitignore"]);
    git(
        &repo.root,
        &[
            "-c",
            "user.name=GT Test",
            "-c",
            "user.email=gt@test",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-m",
            "track gitignore",
        ],
    );

    let agents = format!(
        "{}, {}, {}",
        fixture.fixer.display(),
        fixture.hallucinator.display(),
        fixture.broken.display()
    );
    let output = gt(&repo.root)
        .args([
            "swarm",
            "Fix the race condition and prove it with the suite",
            "-n",
            "3",
            "--agents",
            &agents,
            "--test-cmd",
            &fixture.test_cmd.display().to_string(),
            "--no-ui",
            "--timeout-secs",
            "60",
        ])
        .timeout(Duration::from_secs(120))
        .output()
        .expect("gt swarm runs");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "gt swarm failed: {stdout}{stderr}");

    // Task banner reported the winner: the correct fix (variant c1).
    let task_short = stdout
        .lines()
        .find_map(|l| l.split("Task initialized: ").nth(1))
        .and_then(|rest| rest.split_whitespace().next())
        .expect("task id short in swarm output")
        .to_string();
    assert!(
        stdout.contains("Winner detected"),
        "winner banner missing: {stdout}{stderr}"
    );
    assert!(
        stdout.contains("(c1,")
            || stdout.contains("c1 (")
            || stdout.contains("Winner detected: c1"),
        "expected variant c1 to win: {stdout}"
    );
    assert!(stdout.contains("gt merge"), "merge hint missing: {stdout}");

    // Pre-purge state: COMPLETED task, winner recorded, scores populated.
    let list = gt(&repo.root).args(["list", "--json", "--all"]).output()?;
    let snapshot: serde_json::Value =
        serde_json::from_slice(&list.stdout).expect("gt list --json parses");
    let tasks = snapshot["tasks"].as_array().expect("tasks array");
    assert_eq!(tasks.len(), 1, "exactly one task before purge");
    let winner = tasks[0]["winner_candidate_id"]
        .as_str()
        .expect("winner recorded")
        .to_string();
    assert_eq!(
        tasks[0]["status"], "COMPLETED",
        "arbitration finished the task: {tasks:?}"
    );
    let candidates = tasks[0]["candidates"].as_array().expect("candidates");
    assert_eq!(candidates.len(), 3);
    let winner_entry = candidates
        .iter()
        .find(|c| c["id"].as_str() == Some(winner.as_str()))
        .expect("winner in candidate list");
    assert_eq!(winner_entry["variant_name"], "c1");
    assert_eq!(winner_entry["status"], "READY");
    assert!(
        winner_entry["score"].as_f64().unwrap() > 40.0,
        "correct fix outscores the capped failures: {winner_entry}"
    );
    let hallucination = candidates
        .iter()
        .find(|c| c["variant_name"] == "c2")
        .expect("c2");
    assert_eq!(hallucination["status"], "FAILED");
    assert!(
        hallucination["test_results"]["failed"].as_u64().unwrap() == 3,
        "harness failures parsed into test_results: {hallucination}"
    );
    let churn = hallucination["diff_stat"]["insertions"].as_u64().unwrap();
    let clean = winner_entry["diff_stat"]["files_changed"].as_u64().unwrap();
    assert!(churn > 100, "hallucination should carry big churn: {churn}");
    assert_eq!(clean, 2, "surgical fix touches two files");
    let broken = candidates
        .iter()
        .find(|c| c["variant_name"] == "c3")
        .expect("c3");
    assert_eq!(broken["status"], "FAILED");
    assert_eq!(broken["exit_code"], 3);

    // gt diff: c1's patch vs base, unified and with --stat.
    let diff = gt(&repo.root).args(["diff", "c1"]).output()?.stdout;
    let diff_text = String::from_utf8_lossy(&diff);
    assert!(diff_text.contains("diff --git a/src.txt"), "{diff_text}");
    assert!(diff_text.contains("+fixed"), "{diff_text}");
    let stat = gt(&repo.root)
        .args(["diff", "c1", "--stat"])
        .output()?
        .stdout;
    let stat_json: serde_json::Value = serde_json::from_slice(&stat)?;
    assert_eq!(stat_json["files_changed"], 2, "{stat_json}");

    // Dirty-index protection (SPEC §8): an uncommitted file must make the
    // merge refuse until --autostash (or a clean tree) is provided.
    std::fs::write(repo.root.join("scratch.txt"), "uncommitted\n").expect("dirty scratch");
    let dirty_merge = gt(&repo.root)
        .args(["merge", "c1"])
        .timeout(Duration::from_secs(30))
        .output()
        .expect("dirty gt merge runs");
    let dirty_out = format!(
        "{}{}",
        String::from_utf8_lossy(&dirty_merge.stdout),
        String::from_utf8_lossy(&dirty_merge.stderr)
    );
    assert!(
        !dirty_merge.status.success(),
        "dirty index must refuse the merge: {dirty_out}"
    );
    assert!(dirty_out.contains("autostash"), "{dirty_out}");
    std::fs::remove_file(repo.root.join("scratch.txt")).expect("clean scratch");

    // gt merge c1 promotes the fix with a clean merge commit.
    let merge = gt(&repo.root)
        .args(["merge", "c1"])
        .timeout(Duration::from_secs(30))
        .output()
        .expect("gt merge runs");
    let merge_out = format!(
        "{}{}",
        String::from_utf8_lossy(&merge.stdout),
        String::from_utf8_lossy(&merge.stderr)
    );
    assert!(merge.status.success(), "gt merge failed: {merge_out}");
    assert!(merge_out.contains("Merged c1"), "{merge_out}");

    // Clean history: HEAD has two parents (no-ff merge), the candidate
    // commit is an ancestor, and the fix landed in the primary worktree.
    assert_eq!(
        git(&repo.root, &["symbolic-ref", "--short", "HEAD"]),
        "main"
    );
    let parents = git(&repo.root, &["log", "-1", "--format=%P"]);
    assert_eq!(parents.split_whitespace().count(), 2, "{parents}");
    let found = git(&repo.root, &["log", "--format=%s"]);
    assert!(
        found.contains("fix race condition in pool"),
        "candidate commit not reachable from main: {found}"
    );
    let merged = git(&repo.root, &["log", "-1", "--format=%s"]);
    assert!(
        merged.starts_with("ghosttree(task): merge variant c1"),
        "merge commit message: {merged}"
    );
    assert!(
        String::from_utf8_lossy(
            &std::fs::read(repo.root.join("src.txt")).expect("src.txt after merge")
        )
        .contains("fixed"),
        "fix contents promoted to the primary worktree"
    );

    // Candidate is MERGED in the row set.
    let list = gt(&repo.root).args(["list", "--json", "--all"]).output()?;
    let snapshot: serde_json::Value = serde_json::from_slice(&list.stdout)?;
    assert_eq!(
        snapshot["tasks"][0]["candidates"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["variant_name"] == "c1")
            .unwrap()["status"],
        "MERGED"
    );

    // gt purge removes worktrees, branches, and the task row.
    let purge = gt(&repo.root)
        .args(["purge", &task_short])
        .output()
        .expect("gt purge runs");
    let purge_out = format!(
        "{}{}",
        String::from_utf8_lossy(&purge.stdout),
        String::from_utf8_lossy(&purge.stderr)
    );
    assert!(purge.status.success(), "gt purge failed: {purge_out}");
    assert!(purge_out.contains("purged task"), "{purge_out}");
    assert!(purge_out.contains("0 leaked worktree(s)"), "{purge_out}");

    // Zero leaked worktrees (SPEC §10.1).
    let porcelain = git(&repo.root, &["worktree", "list", "--porcelain"]);
    assert_eq!(
        porcelain
            .lines()
            .filter(|l| l.starts_with("worktree "))
            .count(),
        1,
        "only the primary worktree may remain: {porcelain}"
    );
    let branch_list = git(&repo.root, &["branch", "--list", "ghosttree/*"]);
    assert!(
        branch_list.trim().is_empty(),
        "candidate branches deleted: {branch_list:?}"
    );
    let tasks_left = gt(&repo.root).args(["list", "--json", "--all"]).output()?;
    let snapshot: serde_json::Value = serde_json::from_slice(&tasks_left.stdout)?;
    assert_eq!(
        snapshot["tasks"].as_array().map(Vec::len),
        Some(0),
        "task rows dropped by purge"
    );

    // Stop the auto-started daemon so the machine is clean afterwards.
    let stop = gt(&repo.root).args(["daemon", "stop"]).output()?;
    assert!(
        stop.status.success(),
        "gt daemon stop: {}",
        String::from_utf8_lossy(&stop.stderr)
    );
    wait_gone(
        &repo.root.join(".ghosttree/ghosttree.sock"),
        Duration::from_secs(10),
        "daemon socket",
    );
    wait_gone(
        &repo.root.join(".ghosttree/daemon.pid"),
        Duration::from_secs(10),
        "daemon pid file",
    );
    Ok(())
}

#[test]
fn swarm_rejects_mismatched_agent_counts() {
    let repo = TestRepo::new();
    gt(&repo.root).arg("init").assert().success();
    gt(&repo.root)
        .args(["swarm", "p", "-n", "3", "--agents", "a,b"])
        .assert()
        .failure()
        .stderr(contains("--agents lists 2 command(s) but -n 3"));
}
