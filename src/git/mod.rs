//! Git plumbing abstraction (SPEC §4).
//!
//! All interaction with git goes through low-level plumbing commands invoked
//! with explicit argv vectors — never through a shell and never through
//! high-level interactive commands such as `git checkout` or `git pull`.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use thiserror::Error;

/// Errors surfaced by the git plumbing layer.
#[derive(Debug, Error)]
pub enum GitError {
    #[error("not inside a git repository")]
    NotARepository,
    #[error("git `{cmd}` failed: {stderr}")]
    CommandFailed { cmd: String, stderr: String },
    #[error("failed to parse git output: {0}")]
    Parse(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, GitError>;

/// Handle on the primary repository (PR, SPEC §2): the root worktree of the
/// git repository where `gt init` was run.
#[derive(Debug, Clone)]
pub struct Repo {
    root: PathBuf,
}

/// A single entry from `git worktree list --porcelain`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeEntry {
    /// Absolute path of the worktree.
    pub path: PathBuf,
    /// Commit the worktree HEAD points at (`None` for a bare worktree).
    pub head: Option<String>,
    /// Full ref name of the checked out branch (`None` when detached).
    pub branch: Option<String>,
    pub detached: bool,
    pub bare: bool,
    /// `locked` worktrees with an optional reason (see `git worktree lock`).
    pub locked_reason: Option<String>,
    /// `prunable` worktrees with their reason string.
    pub prunable_reason: Option<String>,
}

/// Result of a real (ort) three-way merge dry-run via `git merge-tree --write-tree`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeTreeResult {
    /// The tree OID that would result from the merge (written even on conflict).
    pub tree_oid: String,
    /// True when the merge would produce conflicts.
    pub conflicted: bool,
    /// Remaining porcelain lines: informational messages and conflict sections.
    pub conflict_output: Vec<String>,
}

/// Summary of `git diff --numstat` between two revisions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NumStat {
    pub files_changed: u32,
    pub insertions: u64,
    pub deletions: u64,
}

impl Repo {
    /// Discover the repository containing `from` (searching upward).
    pub fn discover(from: &Path) -> Result<Self> {
        let out = run_git(from, &["rev-parse", "--show-toplevel"])?;
        let stderr = String::from_utf8_lossy(&out.stderr);
        if !out.status.success() {
            if stderr.contains("not a git repository") || stderr.contains("not a work tree") {
                return Err(GitError::NotARepository);
            }
            return Err(GitError::CommandFailed {
                cmd: "git rev-parse --show-toplevel".into(),
                stderr: stderr.into_owned(),
            });
        }
        let root = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if root.is_empty() {
            return Err(GitError::Parse("empty --show-toplevel output".into()));
        }
        Ok(Self {
            root: PathBuf::from(root),
        })
    }

    /// Root directory of the primary repository.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Resolve a revision to a full commit hash (SPEC §4.1 step 1).
    pub fn rev_parse(&self, rev: &str) -> Result<String> {
        let spec = format!("{rev}^{{commit}}");
        let out = git_out(&self.root, &["rev-parse", "--verify", &spec])?;
        let sha = out.trim().to_string();
        if sha.is_empty() {
            return Err(GitError::Parse(format!(
                "rev-parse produced no output for {rev:?}"
            )));
        }
        Ok(sha)
    }

    /// Current HEAD commit of the primary repository.
    pub fn head_commit(&self) -> Result<String> {
        self.rev_parse("HEAD")
    }

    /// Short name of the current branch, or `None` on a detached HEAD.
    pub fn current_branch(&self) -> Result<Option<String>> {
        let out = run_git(&self.root, &["symbolic-ref", "--short", "-q", "HEAD"])?;
        match out.status.code() {
            Some(0) => {
                let branch = String::from_utf8_lossy(&out.stdout).trim().to_string();
                Ok((!branch.is_empty()).then_some(branch))
            }
            // `symbolic-ref -q` exits 1 when HEAD is detached.
            Some(1) => Ok(None),
            _ => Err(GitError::CommandFailed {
                cmd: "git symbolic-ref --short -q HEAD".into(),
                stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            }),
        }
    }

    /// Verify worktree capability and list all worktrees (SPEC §4.1 step 2).
    pub fn worktree_list(&self) -> Result<Vec<WorktreeEntry>> {
        let out = git_out(&self.root, &["worktree", "list", "--porcelain"])?;
        parse_worktree_list(&out)
    }

    /// Provision an isolated ghost worktree (SPEC §4.1 steps 4–5).
    ///
    /// Atomically creates branch `branch` at `base_commit` and registers a
    /// worktree for it at `<root>/<rel_path>` with `--no-checkout`, then
    /// populates index and files with `reset --hard <base_commit>`.
    /// Fails if the branch already exists.
    pub fn worktree_add(
        &self,
        branch: &str,
        rel_path: &Path,
        base_commit: &str,
    ) -> Result<PathBuf> {
        ensure_safe_ref(branch)?;
        ensure_safe_arg("worktree path", rel_path.to_str())?;
        ensure_safe_arg("base commit", Some(base_commit))?;
        let rel = rel_path
            .to_str()
            .ok_or_else(|| GitError::Parse("worktree path is not valid UTF-8".into()))?;
        git_out(
            &self.root,
            &[
                "worktree",
                "add",
                "--no-checkout",
                "-b",
                branch,
                rel,
                base_commit,
            ],
        )?;
        let wt_path = self.root.join(rel_path);
        git_out(&wt_path, &["reset", "--hard", base_commit])?;
        Ok(wt_path)
    }

    /// Remove a worktree; `force` overrides the dirty-worktree refusal.
    pub fn worktree_remove(&self, rel_path: &Path, force: bool) -> Result<()> {
        let rel = rel_path
            .to_str()
            .ok_or_else(|| GitError::Parse("worktree path is not valid UTF-8".into()))?;
        if force {
            git_out(&self.root, &["worktree", "remove", "--force", rel])?;
        } else {
            git_out(&self.root, &["worktree", "remove", rel])?;
        }
        Ok(())
    }

    /// Prune stale worktree administrative files (SPEC §8 orphan recovery).
    pub fn worktree_prune(&self) -> Result<()> {
        git_out(&self.root, &["worktree", "prune"]).map(drop)
    }

    /// HEAD of a ghost worktree, used by the daemon to detect agent commits.
    pub fn worktree_head(wt_path: &Path) -> Result<String> {
        let out = git_out(wt_path, &["rev-parse", "HEAD"])?;
        let sha = out.trim().to_string();
        if sha.is_empty() {
            return Err(GitError::Parse("empty worktree HEAD".into()));
        }
        Ok(sha)
    }

    /// Committed diff statistics `from..to` (arbitrator churn input, SPEC §7).
    pub fn diff_numstat(&self, from: &str, to: &str) -> Result<NumStat> {
        ensure_safe_arg("from revision", Some(from))?;
        ensure_safe_arg("to revision", Some(to))?;
        let out = git_out(&self.root, &["diff", "--numstat", &format!("{from}..{to}")])?;
        parse_numstat(&out)
    }

    /// Unified patch of `from..to` (SPEC §5.1 `gt diff`). Returns the raw
    /// git output verbatim; an empty diff is an empty string.
    pub fn diff_patch(&self, from: &str, to: &str) -> Result<String> {
        ensure_safe_arg("from revision", Some(from))?;
        ensure_safe_arg("to revision", Some(to))?;
        git_out(&self.root, &["diff", &format!("{from}..{to}")])
    }

    /// True when the worktree/index of `cwd` (the primary repo unless a
    /// ghost worktree is passed) holds uncommitted changes (SPEC §8 merge
    /// safety check).
    pub fn is_dirty(&self) -> Result<bool> {
        let out = run_git(&self.root, &["status", "--porcelain"])?;
        if !out.status.success() {
            return Err(GitError::CommandFailed {
                cmd: "git status --porcelain".into(),
                stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            });
        }
        let stdout = String::from_utf8_lossy(&out.stdout);
        Ok(!stdout.trim().is_empty())
    }

    /// Make a dangling stash commit capturing the current dirty state
    /// without touching the working tree (SPEC §8 autostash path), then
    /// keep it in the stash ref. Returns the stash commit sha.
    pub fn stash_autostash(&self, message: &str) -> Result<String> {
        let sha = git_out(&self.root, &["stash", "create"])?
            .trim()
            .to_string();
        if sha.is_empty() {
            return Err(GitError::Parse("stash create produced no commit".into()));
        }
        git_out(&self.root, &["stash", "store", "-m", message, &sha])?;
        Ok(sha)
    }

    /// Merge a candidate branch into the ACTIVE branch with a guaranteed
    /// merge commit (SPEC §4.3 final step).
    pub fn merge_no_ff(&self, branch: &str, message: &str) -> Result<()> {
        ensure_safe_ref(branch)?;
        git_out(&self.root, &["merge", "--no-ff", "-m", message, branch])?;
        Ok(())
    }

    /// Stage a squash merge of `branch` into the active branch; the result
    /// stays in the index for [`Repo::commit_index`].
    pub fn merge_squash(&self, branch: &str) -> Result<()> {
        ensure_safe_ref(branch)?;
        git_out(&self.root, &["merge", "--squash", branch])?;
        Ok(())
    }

    /// True when the index holds staged changes (`git diff --cached --quiet`
    /// exit 1); commit is skipped when a squash merged identical content.
    pub fn staged_changes(&self) -> Result<bool> {
        let out = run_git(&self.root, &["diff", "--cached", "--quiet"])?;
        match out.status.code() {
            Some(0) => Ok(false),
            Some(1) => Ok(true),
            _ => Err(GitError::CommandFailed {
                cmd: "git diff --cached --quiet".into(),
                stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            }),
        }
    }

    /// Commit whatever the index holds (finishes `merge --squash`), then
    /// return the new HEAD.
    pub fn commit_index(&self, message: &str) -> Result<String> {
        git_out(&self.root, &["commit", "-m", message])?;
        self.rev_parse("HEAD")
    }

    /// Delete a branch ref regardless of merge state (`gt purge` cleanup;
    /// the caller decides whether discarding unmerged work is acceptable).
    pub fn delete_branch(&self, branch: &str) -> Result<()> {
        ensure_safe_ref(branch)?;
        git_out(&self.root, &["branch", "-D", branch]).map(drop)
    }

    /// Dry-run a three-way merge of `theirs` into `ours` (SPEC §4.3).
    ///
    /// SPEC §4.3 lists `git merge-tree --write-tree <base> HEAD <winner>`,
    /// but `--write-tree` mode takes exactly two commits; `--merge-base` is
    /// the plumbing-correct way to pin the common ancestor, so the base is
    /// supplied as `--merge-base=<base>` instead of a positional argument.
    pub fn merge_tree(&self, base: &str, ours: &str, theirs: &str) -> Result<MergeTreeResult> {
        let out = run_git(
            &self.root,
            &[
                "merge-tree",
                "--write-tree",
                &format!("--merge-base={base}"),
                ours,
                theirs,
            ],
        )?;
        let code = out.status.code().unwrap_or(-1);
        let stdout = String::from_utf8_lossy(&out.stdout);
        let mut lines = stdout.lines();
        let tree_oid = lines.next().unwrap_or("").trim().to_string();
        if tree_oid.is_empty() {
            return Err(GitError::CommandFailed {
                cmd: "git merge-tree --write-tree".into(),
                stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            });
        }
        let conflict_output = lines.map(str::to_string).collect();
        match code {
            0 => Ok(MergeTreeResult {
                tree_oid,
                conflicted: false,
                conflict_output,
            }),
            1 => Ok(MergeTreeResult {
                tree_oid,
                conflicted: true,
                conflict_output,
            }),
            _ => Err(GitError::CommandFailed {
                cmd: format!("git merge-tree --write-tree --merge-base={base}"),
                stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            }),
        }
    }
}

/// Run `git -C <cwd> <args>` and capture output. Never goes through a shell.
fn run_git(cwd: &Path, args: &[&str]) -> Result<Output> {
    Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .output()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                GitError::Parse("git executable not found in PATH".into())
            } else {
                GitError::Io(e)
            }
        })
}

/// Run a git command, returning stdout on success; errors carry stderr.
fn git_out(cwd: &Path, args: &[&str]) -> Result<String> {
    let out = run_git(cwd, args)?;
    if !out.status.success() {
        return Err(GitError::CommandFailed {
            cmd: format!("git {}", args.join(" ")),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        });
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Reject argv values that could be mistaken for git options.
fn ensure_safe_arg(label: &str, value: Option<&str>) -> Result<()> {
    let value = value.ok_or_else(|| GitError::Parse(format!("{label} is not valid UTF-8")))?;
    if value.starts_with('-') {
        return Err(GitError::Parse(format!(
            "{label} must not start with '-': {value}"
        )));
    }
    Ok(())
}

/// Branch/ref names must be valid and cannot look like options.
fn ensure_safe_ref(branch: &str) -> Result<()> {
    ensure_safe_arg("branch name", Some(branch))
}

/// Parse `git worktree list --porcelain` output into structured entries.
pub fn parse_worktree_list(output: &str) -> Result<Vec<WorktreeEntry>> {
    let mut entries = Vec::new();
    for record in output.split("\n\n") {
        if record.trim().is_empty() {
            continue;
        }
        let mut entry = WorktreeEntry {
            path: PathBuf::new(),
            head: None,
            branch: None,
            detached: false,
            bare: false,
            locked_reason: None,
            prunable_reason: None,
        };
        for line in record.lines() {
            if line.is_empty() {
                continue;
            }
            let (key, value) = line.split_once(' ').unwrap_or((line, ""));
            match key {
                "worktree" => {
                    let path = unquote_c_style(value)?;
                    entry.path = PathBuf::from(path);
                }
                "HEAD" => entry.head = Some(value.to_string()),
                "branch" => entry.branch = Some(value.to_string()),
                "detached" => entry.detached = true,
                "bare" => entry.bare = true,
                "locked" => entry.locked_reason = (!value.is_empty()).then(|| value.to_string()),
                "prunable" => {
                    entry.prunable_reason = (!value.is_empty()).then(|| value.to_string())
                }
                // Unknown keys are ignored for forward compatibility.
                _ => {}
            }
        }
        if entry.path.as_os_str().is_empty() {
            return Err(GitError::Parse(
                "worktree record is missing the 'worktree' line".into(),
            ));
        }
        entries.push(entry);
    }
    Ok(entries)
}

/// Undo git's C-style path quoting (`"tab\there"`, octal byte escapes).
/// Unquoted input is returned unchanged.
fn unquote_c_style(s: &str) -> Result<String> {
    let bytes = s.as_bytes();
    if !(bytes.len() >= 2 && bytes[0] == b'"' && bytes[bytes.len() - 1] == b'"') {
        return Ok(s.to_string());
    }
    let inner = &bytes[1..bytes.len() - 1];
    let mut out: Vec<u8> = Vec::with_capacity(inner.len());
    let mut i = 0;
    while i < inner.len() {
        let b = inner[i];
        if b != b'\\' {
            out.push(b);
            i += 1;
            continue;
        }
        i += 1;
        let Some(&esc) = inner.get(i) else {
            return Err(GitError::Parse(format!(
                "dangling escape in quoted path: {s}"
            )));
        };
        i += 1;
        match esc {
            b'n' => out.push(b'\n'),
            b't' => out.push(b'\t'),
            b'r' => out.push(b'\r'),
            b'"' => out.push(b'"'),
            b'\\' => out.push(b'\\'),
            b'a' => out.push(0x07),
            b'b' => out.push(0x08),
            b'f' => out.push(0x0c),
            b'v' => out.push(0x0b),
            b'0'..=b'7' => {
                // Octal byte escapes are 1..=3 digits; git emits at most 3.
                let mut val = u32::from(esc - b'0');
                let mut digits = 1;
                while digits < 3 {
                    match inner.get(i) {
                        Some(&n @ b'0'..=b'7') => {
                            val = val * 8 + u32::from(n - b'0');
                            i += 1;
                            digits += 1;
                        }
                        _ => break,
                    }
                }
                if val > u32::from(u8::MAX) {
                    return Err(GitError::Parse(format!(
                        "octal escape out of range in quoted path: {s}"
                    )));
                }
                out.push(val as u8);
            }
            other => {
                return Err(GitError::Parse(format!(
                    "unexpected escape '\\{}' in quoted path: {s}",
                    char::from(other)
                )));
            }
        }
    }
    String::from_utf8(out)
        .map_err(|_| GitError::Parse(format!("quoted path is not valid UTF-8: {s}")))
}

/// Parse `git diff --numstat` output: `insertions<TAB>deletions<TAB>path`
/// per changed file; binary files report `-` for both counts.
pub fn parse_numstat(output: &str) -> Result<NumStat> {
    let mut stat = NumStat {
        files_changed: 0,
        insertions: 0,
        deletions: 0,
    };
    for line in output.lines() {
        if line.is_empty() {
            continue;
        }
        let mut fields = line.split('\t');
        let ins = fields.next().unwrap_or("");
        let del = fields.next().unwrap_or("");
        if fields.next().is_none() {
            return Err(GitError::Parse(format!(
                "malformed numstat line {line:?} (want `ins\\tdel\\tpath`)"
            )));
        }
        stat.files_changed += 1;
        // Binary entries (`-`) count as changed files but contribute no lines.
        stat.insertions += parse_numstat_field(ins, line)?;
        stat.deletions += parse_numstat_field(del, line)?;
    }
    Ok(stat)
}

/// One numstat count column: a decimal number or the binary marker `-`.
fn parse_numstat_field(field: &str, line: &str) -> Result<u64> {
    if field == "-" {
        return Ok(0);
    }
    field
        .parse::<u64>()
        .map_err(|_| GitError::Parse(format!("malformed numstat count {field:?} in {line:?}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_numstat_sums_files_and_lines() {
        let out = "10\t2\tsrc/a.rs\n0\t0\tbinary.png\n3\t-\tdata.bin\n";
        let stat = parse_numstat(out).unwrap();
        assert_eq!(stat.files_changed, 3);
        assert_eq!(stat.insertions, 13);
        assert_eq!(stat.deletions, 2);
    }

    #[test]
    fn parse_numstat_empty_diff() {
        let stat = parse_numstat("").unwrap();
        assert_eq!(
            stat,
            NumStat {
                files_changed: 0,
                insertions: 0,
                deletions: 0
            }
        );
    }

    #[test]
    fn parse_numstat_rejects_malformed_counts() {
        let err = parse_numstat("x\t2\tfile\n").unwrap_err();
        assert!(err.to_string().contains("malformed numstat count"));
        let err = parse_numstat("1\t2\n").unwrap_err();
        assert!(err.to_string().contains("malformed numstat line"));
    }

    #[test]
    fn parse_porcelain_full_record_set() {
        let out = "\
worktree /repo
HEAD 0123abcd
branch refs/heads/main

worktree /repo/.ghosttree/worktrees/t1
HEAD 0123abcd
detached

worktree /repo/.ghosttree/worktrees/t2
HEAD 0123abcd
branch refs/heads/ghosttree/task/v2
locked consistency reason

worktree /repo/.ghosttree/worktrees/t3
HEAD 0123abcd
branch refs/heads/ghosttree/task/v3
prunable git status failed
";
        let entries = parse_worktree_list(out).unwrap();
        assert_eq!(entries.len(), 4);

        assert_eq!(entries[0].path, PathBuf::from("/repo"));
        assert_eq!(entries[0].head.as_deref(), Some("0123abcd"));
        assert_eq!(entries[0].branch.as_deref(), Some("refs/heads/main"));
        assert!(!entries[0].detached);
        assert!(!entries[0].bare);

        assert!(entries[1].detached);
        assert_eq!(entries[1].branch, None);

        assert_eq!(
            entries[2].branch.as_deref(),
            Some("refs/heads/ghosttree/task/v2")
        );
        assert_eq!(
            entries[2].locked_reason.as_deref(),
            Some("consistency reason")
        );

        assert_eq!(
            entries[3].prunable_reason.as_deref(),
            Some("git status failed")
        );
    }

    #[test]
    fn parse_porcelain_bare_repository() {
        let entries = parse_worktree_list("worktree /srv/bare\nbare\n").unwrap();
        assert_eq!(entries.len(), 1);
        assert!(entries[0].bare);
        assert_eq!(entries[0].head, None);
        assert_eq!(entries[0].branch, None);
    }

    #[test]
    fn parse_porcelain_locked_without_reason() {
        let entries =
            parse_worktree_list("worktree /wt\nHEAD abc\nbranch refs/heads/x\nlocked\n").unwrap();
        assert!(entries[0].locked_reason.is_none());
    }

    #[test]
    fn parse_porcelain_missing_worktree_line_is_an_error() {
        let err = parse_worktree_list("HEAD abc\ndetached\n").unwrap_err();
        assert!(err.to_string().contains("missing the 'worktree' line"));
    }

    #[test]
    fn parse_porcelain_empty_output() {
        assert!(parse_worktree_list("").unwrap().is_empty());
        assert!(parse_worktree_list("\n\n").unwrap().is_empty());
    }

    #[test]
    fn unquote_plain_paths_pass_through() {
        assert_eq!(unquote_c_style("/plain/path").unwrap(), "/plain/path");
        assert_eq!(
            unquote_c_style("/path with space").unwrap(),
            "/path with space"
        );
    }

    #[test]
    fn unquote_escapes_and_octal_bytes() {
        assert_eq!(unquote_c_style("\"tab\\there\"").unwrap(), "tab\there");
        assert_eq!(
            unquote_c_style("\"quote\\\"inside\"").unwrap(),
            "quote\"inside"
        );
        assert_eq!(unquote_c_style("\"back\\\\slash\"").unwrap(), "back\\slash");
        // e-acute as UTF-8 bytes 0xC3 0xA9, each octal-escaped by git.
        assert_eq!(unquote_c_style("\"caf\\303\\251\"").unwrap(), "café");
        assert_eq!(
            unquote_c_style("\"\\010\\011\\012\"").unwrap(),
            "\u{8}\t\n" // octal 010=BS, 011=TAB, 012=LF
        );
    }

    #[test]
    fn unquote_passes_through_unquoted_shape() {
        // Starts with `"` but not properly quoted -> an unquoted literal name.
        assert_eq!(unquote_c_style("\"dangling").unwrap(), "\"dangling");
    }

    #[test]
    fn unquote_rejects_malformed_escapes() {
        // Properly quoted, but the escape sequence itself is invalid.
        assert!(unquote_c_style("\"bad\\x\"").is_err());
        // Properly quoted, but ends on a dangling backslash.
        assert!(unquote_c_style("\"dangling\\\"").is_err());
        // `caf\3777`: octal 377 is a bare 0xFF byte, never valid UTF-8.
        assert!(unquote_c_style("\"caf\\3777\"").is_err());
    }
}
