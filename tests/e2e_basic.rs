//! End-to-end CLI tests: `gt init` / `gt list` on a temporary git repo
//! (SPEC §5.1, §10.1).

mod common;

use assert_cmd::Command as AssertCommand;
use common::TestRepo;
use ghosttree::db::{NewTask, Store, TaskStrategy};
use predicates::prelude::*;
use predicates::str::contains;
use serde_json::Value;
use std::fs;

fn gt() -> AssertCommand {
    AssertCommand::cargo_bin("gt").expect("gt binary built")
}

#[test]
fn init_layout_config_and_gitignore() {
    let repo = TestRepo::new();

    gt().arg("init")
        .current_dir(&repo.root)
        .assert()
        .success()
        .stdout(contains("Initialized GhostTree"));

    assert!(repo.root.join(".ghosttree/ghosttree.db").is_file());
    assert!(repo.root.join(".ghosttree/config.toml").is_file());
    assert!(repo.root.join(".ghosttree/logs").is_dir());
    assert!(repo.root.join(".ghosttree/worktrees").is_dir());
    // The daemon pid file belongs to the daemon (Phase 3), never to init.
    assert!(!repo.root.join(".ghosttree/daemon.pid").exists());

    let gitignore = fs::read_to_string(repo.root.join(".gitignore")).unwrap();
    assert!(
        gitignore.lines().any(|l| l.trim() == ".ghosttree/"),
        ".gitignore must ignore .ghosttree/: {gitignore:?}"
    );
}

/// Never commit `.ghosttree` state into the repository (SPEC §2.1 layout).
#[test]
fn init_runs_twice_without_side_effects() {
    let repo = TestRepo::new();
    gt().arg("init").current_dir(&repo.root).assert().success();
    gt().arg("init").current_dir(&repo.root).assert().success();
    assert!(repo.root.join(".ghosttree/ghosttree.db").is_file());
}

#[test]
fn init_preserves_custom_config_without_force() {
    let repo = TestRepo::new();
    gt().arg("init").current_dir(&repo.root).assert().success();

    fs::write(repo.root.join(".ghosttree/config.toml"), "custom = 1\n").unwrap();
    gt().arg("init").current_dir(&repo.root).assert().success();
    assert_eq!(
        fs::read_to_string(repo.root.join(".ghosttree/config.toml")).unwrap(),
        "custom = 1\n"
    );

    gt().args(["init", "--force"])
        .current_dir(&repo.root)
        .assert()
        .success();
    let restored = fs::read_to_string(repo.root.join(".ghosttree/config.toml")).unwrap();
    assert!(
        restored.contains("[swarm]"),
        "force must restore the template"
    );
}

#[test]
fn init_refuses_non_git_directory() {
    let dir = tempfile::tempdir().unwrap();
    let out = gt().arg("init").current_dir(dir.path()).assert().failure();
    out.stderr(contains("not inside a git repository"));
}

#[test]
fn init_works_from_a_subdirectory_of_the_repo() {
    let repo = TestRepo::new();
    fs::create_dir_all(repo.root.join("sub/dir")).unwrap();
    gt().arg("init")
        .current_dir(repo.root.join("sub/dir"))
        .assert()
        .success();
    // State always lands at the repository root, not the cwd.
    assert!(repo.root.join(".ghosttree/ghosttree.db").is_file());
    assert!(!repo.root.join("sub/dir/.ghosttree").exists());
}

#[test]
fn list_json_is_empty_before_any_swarm() {
    let repo = TestRepo::new();
    gt().arg("init").current_dir(&repo.root).assert().success();

    let stdout = gt()
        .args(["list", "--json"])
        .current_dir(&repo.root)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let payload: Value = serde_json::from_slice(&stdout).expect("valid JSON");
    assert_eq!(payload["tasks"].as_array().unwrap().len(), 0);
}

#[test]
fn list_json_includes_tasks_and_candidates() {
    let repo = TestRepo::new();
    gt().arg("init").current_dir(&repo.root).assert().success();

    let db_path = repo.root.join(".ghosttree/ghosttree.db");
    let store = Store::open(&db_path).unwrap();
    store
        .insert_task(&NewTask {
            id: "01JMW8KAAAAAAAAAAAAAAAAAAA".into(),
            title: "Fix race condition in connection pool".into(),
            prompt: "fix it".into(),
            base_commit: "7b31cf2".into(),
            base_branch: "main".into(),
            strategy: TaskStrategy::Competing,
        })
        .unwrap();
    store
        .insert_task(&NewTask {
            id: "01JMW90AAAAAAAAAAAAAAAAAA".into(),
            title: "completed one".into(),
            prompt: "done".into(),
            base_commit: "7b31cf2".into(),
            base_branch: "main".into(),
            strategy: TaskStrategy::Decomposed,
        })
        .unwrap();
    // Mark the second task terminal: hidden without --all.
    store
        .update_task_status(
            "01JMW90AAAAAAAAAAAAAAAAAA",
            ghosttree::db::TaskStatus::Completed,
        )
        .unwrap();

    // Active only.
    let stdout = gt()
        .args(["list", "--json"])
        .current_dir(&repo.root)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let payload: Value = serde_json::from_slice(&stdout).unwrap();
    let tasks = payload["tasks"].as_array().unwrap();
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0]["id"], "01JMW8KAAAAAAAAAAAAAAAAAAA");
    assert_eq!(tasks[0]["status"], "PENDING");
    assert_eq!(tasks[0]["candidates"].as_array().unwrap().len(), 0);

    // --all includes the terminal task.
    let stdout = gt()
        .args(["list", "--json", "--all"])
        .current_dir(&repo.root)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let payload: Value = serde_json::from_slice(&stdout).unwrap();
    assert_eq!(payload["tasks"].as_array().unwrap().len(), 2);
}

#[test]
fn list_text_table_shows_task_summary() {
    let repo = TestRepo::new();
    gt().arg("init").current_dir(&repo.root).assert().success();

    let db_path = repo.root.join(".ghosttree/ghosttree.db");
    let store = Store::open(&db_path).unwrap();
    store
        .insert_task(&NewTask {
            id: "01JMW8KAAAAAAAAAAAAAAAAAAA".into(),
            title: "Fix race condition in connection pool".into(),
            prompt: "fix it".into(),
            base_commit: "7b31cf2".into(),
            base_branch: "main".into(),
            strategy: TaskStrategy::Competing,
        })
        .unwrap();

    gt().arg("list")
        .current_dir(&repo.root)
        .assert()
        .success()
        .stdout(
            contains("TASK")
                .and(contains("STATUS"))
                .and(contains("01JMW8K"))
                .and(contains("PENDING"))
                .and(contains("Fix race condition")),
        );
}

#[test]
fn list_fails_when_not_initialized() {
    let repo = TestRepo::new();
    gt().arg("list")
        .current_dir(&repo.root)
        .assert()
        .failure()
        .stderr(contains("not initialized"));
}

#[test]
fn ghosttree_alias_binary_works() {
    let repo = TestRepo::new();
    AssertCommand::cargo_bin("ghosttree")
        .unwrap()
        .arg("--version")
        .current_dir(&repo.root)
        .assert()
        .success();
}
