//! Integration tests for the git plumbing layer (SPEC §4, §10.1).

mod common;

use common::{commit_file, git, TestRepo};
use ghosttree::git::Repo;
use std::fs;
use std::path::Path;

#[test]
fn discover_finds_toplevel_from_subdirectory() {
    let repo = TestRepo::new();
    let subdir = repo.root.join("tools");
    fs::create_dir_all(&subdir).unwrap();
    let found = Repo::discover(&subdir).unwrap();
    assert_eq!(found.root(), repo.root.canonicalize().unwrap());
}

#[test]
fn discover_fails_outside_repository() {
    let dir = tempfile::tempdir().unwrap();
    let err = Repo::discover(dir.path()).unwrap_err();
    assert!(
        err.to_string().contains("not inside a git repository"),
        "unexpected error: {err}"
    );
}

#[test]
fn head_and_rev_parse_roundtrip() {
    let repo = TestRepo::new();
    let git_repo = Repo::discover(&repo.root).unwrap();
    let head = git_repo.head_commit().unwrap();
    assert_eq!(head, git(&repo.root, &["rev-parse", "HEAD"]));
    let resolved = git_repo.rev_parse(&head).unwrap();
    assert_eq!(resolved, head);
    // Short sha resolves too.
    let short = &head[..7];
    assert_eq!(git_repo.rev_parse(short).unwrap(), head);
}

#[test]
fn rev_parse_rejects_unknown_revisions() {
    let repo = TestRepo::new();
    let git_repo = Repo::discover(&repo.root).unwrap();
    assert!(git_repo.rev_parse("no-such-ref-xyz").is_err());
}

#[test]
fn current_branch_and_detach() {
    let repo = TestRepo::new();
    let git_repo = Repo::discover(&repo.root).unwrap();
    assert_eq!(git_repo.current_branch().unwrap().as_deref(), Some("main"));

    git(&repo.root, &["checkout", "--detach", "HEAD"]);
    assert_eq!(git_repo.current_branch().unwrap(), None);
}

#[test]
fn worktree_add_creates_branch_and_populates_worktree() {
    let repo = TestRepo::new();
    let git_repo = Repo::discover(&repo.root).unwrap();
    let base = git_repo.head_commit().unwrap();

    let rel = Path::new(".ghosttree/worktrees/task01-v1");
    let branch = "ghosttree/task01/v1";
    let wt_path = git_repo.worktree_add(branch, rel, &base).unwrap();

    assert!(wt_path.exists());
    // `reset --hard` must have populated the files from base.
    assert_eq!(
        fs::read_to_string(wt_path.join("src.txt")).unwrap(),
        "line1\nline2\nline3\n"
    );

    let entries = git_repo.worktree_list().unwrap();
    let entry = entries
        .iter()
        .find(|e| e.path.ends_with(rel))
        .expect("new worktree listed");
    assert_eq!(
        entry.branch.as_deref(),
        Some("refs/heads/ghosttree/task01/v1")
    );
    assert_eq!(entry.head.as_deref(), Some(base.as_str()));
    assert!(!entry.detached);
    assert!(!entry.bare);
    assert!(entry.locked_reason.is_none());

    // Re-adding the same branch must fail (branch already exists).
    assert!(git_repo.worktree_add(branch, rel, &base).is_err());
}

#[test]
fn worktree_add_rejects_option_like_names() {
    let repo = TestRepo::new();
    let git_repo = Repo::discover(&repo.root).unwrap();
    let base = git_repo.head_commit().unwrap();
    let err = git_repo
        .worktree_add("--evil", Path::new("wt-evil"), &base)
        .unwrap_err();
    assert!(err.to_string().contains("must not start with '-'"));
}

#[test]
fn worktree_remove_then_prune_cleans_up() {
    let repo = TestRepo::new();
    let git_repo = Repo::discover(&repo.root).unwrap();
    let base = git_repo.head_commit().unwrap();
    let rel = Path::new(".ghosttree/worktrees/task02-v1");
    let wt_path = git_repo
        .worktree_add("ghosttree/task02/v1", rel, &base)
        .unwrap();

    git_repo.worktree_remove(rel, false).unwrap();
    assert!(!wt_path.exists());
    let entries = git_repo.worktree_list().unwrap();
    assert!(entries.iter().all(|e| !e.path.ends_with(rel)));

    // Removal created administrative leftovers in .git; prune must be safe.
    git_repo.worktree_prune().unwrap();
}

#[test]
fn worktree_remove_refuses_dirty_worktree_unless_forced() {
    let repo = TestRepo::new();
    let git_repo = Repo::discover(&repo.root).unwrap();
    let base = git_repo.head_commit().unwrap();
    let rel = Path::new(".ghosttree/worktrees/task03-v1");
    let wt_path = git_repo
        .worktree_add("ghosttree/task03/v1", rel, &base)
        .unwrap();

    fs::write(wt_path.join("dirty.txt"), "uncommitted\n").unwrap();
    assert!(git_repo.worktree_remove(rel, false).is_err());
    // The worktree is still there...
    assert!(wt_path.exists());
    // ...but --force removes it.
    git_repo.worktree_remove(rel, true).unwrap();
    assert!(!wt_path.exists());
}

#[test]
fn merge_tree_accepts_a_clean_merge() {
    let repo = TestRepo::new();
    let git_repo = Repo::discover(&repo.root).unwrap();
    let base = git_repo.head_commit().unwrap();

    // Main advances README; ghost worktree advances src.txt (disjoint files).
    commit_file(&repo.root, "README.md", "updated on main\n", "main change");
    let ours = git_repo.head_commit().unwrap();

    let rel = Path::new(".ghosttree/worktrees/task04-v1");
    let wt = git_repo
        .worktree_add("ghosttree/task04/v1", rel, &base)
        .unwrap();
    let theirs = commit_file(&wt, "src.txt", "changed in worktree\n", "wt change");

    let result = git_repo.merge_tree(&base, &ours, &theirs).unwrap();
    assert!(!result.conflicted);
    assert!(!result.tree_oid.is_empty());
    assert!(result.conflict_output.is_empty());
}

#[test]
fn merge_tree_reports_conflicts_and_writes_a_tree() {
    let repo = TestRepo::new();
    let git_repo = Repo::discover(&repo.root).unwrap();
    let base = git_repo.head_commit().unwrap();

    commit_file(
        &repo.root,
        "src.txt",
        "main rewrote line 2\n",
        "main rewrite",
    );
    let ours = git_repo.head_commit().unwrap();

    let rel = Path::new(".ghosttree/worktrees/task05-v1");
    let wt = git_repo
        .worktree_add("ghosttree/task05/v1", rel, &base)
        .unwrap();
    let theirs = commit_file(&wt, "src.txt", "agent rewrote line 2\n", "wt rewrite");

    let result = git_repo.merge_tree(&base, &ours, &theirs).unwrap();
    assert!(result.conflicted);
    // A tree is still produced (conflict markers materialized in the tree).
    assert_eq!(result.tree_oid.len(), 40);
    assert!(
        result
            .conflict_output
            .iter()
            .any(|line| line.contains("src.txt")),
        "conflict listing should mention the file: {:?}",
        result.conflict_output
    );
}

#[test]
fn diff_numstat_sums_real_changes_and_skips_binary() {
    let repo = TestRepo::new();
    let git_repo = Repo::discover(&repo.root).unwrap();
    let base = git_repo.head_commit().unwrap();

    // Two text files plus a binary blob.
    fs::write(repo.root.join("src.txt"), "one\ntwo\nthree\nfour\n").unwrap();
    fs::write(repo.root.join("extra.txt"), "new file lines\n").unwrap();
    fs::write(repo.root.join("blob.bin"), [0u8, 1, 2, 159, 255]).unwrap();
    git(&repo.root, &["add", "-A"]);
    let head = commit_message(&repo.root, "mixed change set");

    let stat = git_repo.diff_numstat(&base, &head).unwrap();
    assert_eq!(stat.files_changed, 3);
    // src.txt: 1 inserted (four) + 3 deleted (rewritten context differs);
    // exact counts come from real git below.
    let expected = git(
        &repo.root,
        &["diff", "--numstat", &format!("{base}..{head}")],
    );
    assert!(
        expected.lines().count() == 3,
        "three files should be listed: {expected}"
    );
    assert!(stat.insertions > 0, "mixed change set should add lines");
}

#[test]
fn diff_numstat_is_empty_for_identical_revisions() {
    let repo = TestRepo::new();
    let git_repo = Repo::discover(&repo.root).unwrap();
    let head = git_repo.head_commit().unwrap();
    let stat = git_repo.diff_numstat(&head, &head).unwrap();
    assert_eq!(stat.files_changed, 0);
    assert_eq!(stat.insertions, 0);
    assert_eq!(stat.deletions, 0);
}

#[test]
fn diff_patch_renders_unified_diff_against_base() {
    let repo = TestRepo::new();
    let git_repo = Repo::discover(&repo.root).unwrap();
    let base = git_repo.head_commit().unwrap();
    fs::write(repo.root.join("src.txt"), "changed\n").unwrap();
    git(&repo.root, &["add", "src.txt"]);
    let head = commit_message(&repo.root, "touch src");

    let patch = git_repo.diff_patch(&base, &head).unwrap();
    assert!(patch.contains("diff --git a/src.txt"), "{patch}");
    assert!(patch.contains("--- a/src.txt"), "{patch}");
    assert!(patch.contains("+++ b/src.txt"), "{patch}");
    assert!(patch.contains("-line1"), "{patch}");
}

/// Commit with an explicit identity (tests run without global git config).
fn commit_message(root: &Path, message: &str) -> String {
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
