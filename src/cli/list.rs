//! `gt list` (SPEC §5.1): display tasks, candidate counts, and winners.

use std::path::Path;

use anyhow::{bail, Context, Result};

use crate::db::{short_id, views};

/// Maximum characters of a task title shown in the text table.
const TITLE_WIDTH: usize = 48;

struct Row {
    short: String,
    status: String,
    candidates: usize,
    winner: String,
    title: String,
}

pub fn run(json: bool, all: bool) -> Result<()> {
    let repo = crate::git::Repo::discover(Path::new("."))
        .context("gt list must run inside a git repository")?;
    let db_path = repo.root().join(".ghosttree").join("ghosttree.db");
    if !db_path.exists() {
        bail!(
            "GhostTree is not initialized here (missing {}); run `gt init` first",
            db_path.display()
        );
    }
    let store =
        crate::db::Store::open(&db_path).with_context(|| format!("failed to open {db_path:?}"))?;
    let snapshot =
        views::snapshot(&store, all).with_context(|| format!("failed to read {db_path:?}"))?;

    let rows: Vec<Row> = snapshot
        .tasks
        .iter()
        .map(|t| Row {
            short: short_id(&t.id),
            status: t.status.clone(),
            candidates: t.candidates.len(),
            winner: t
                .winner_candidate_id
                .as_deref()
                .map(short_id)
                .unwrap_or_else(|| "-".into()),
            title: one_line_title(&t.title),
        })
        .collect();

    if json {
        let text =
            serde_json::to_string_pretty(&snapshot).context("failed to serialize list output")?;
        println!("{text}");
    } else {
        print_table(&rows);
    }
    Ok(())
}

/// Titles are single-line in practice; collapse any embedded newlines for the table.
fn one_line_title(title: &str) -> String {
    let mut collapsed = title.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() > TITLE_WIDTH {
        collapsed = collapsed.chars().take(TITLE_WIDTH - 3).collect::<String>() + "...";
    }
    collapsed
}

fn print_table(rows: &[Row]) {
    if rows.is_empty() {
        println!("No tasks.");
        return;
    }
    let mut task_w = "TASK".len();
    let mut status_w = "STATUS".len();
    let mut cand_w = "CANDIDATES".len();
    let mut win_w = "WINNER".len();
    for r in rows {
        task_w = task_w.max(r.short.len());
        status_w = status_w.max(r.status.len());
        cand_w = cand_w.max(r.candidates.to_string().len());
        win_w = win_w.max(r.winner.len());
    }
    let header = format!(
        "{:<task_w$}  {:<status_w$}  {:>cand_w$}  {:<win_w$}  TITLE",
        "TASK", "STATUS", "CANDIDATES", "WINNER",
    );
    println!("{header}");
    println!("{}", "=".repeat(header.chars().count()));
    for r in rows {
        println!(
            "{:<task_w$}  {:<status_w$}  {:>cand_w$}  {:<win_w$}  {}",
            r.short, r.status, r.candidates, r.winner, r.title,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn title_collapses_and_truncates() {
        assert_eq!(one_line_title("one  two\nthree"), "one two three");
        let long = "x".repeat(TITLE_WIDTH + 10);
        let cut = one_line_title(&long);
        assert_eq!(cut.chars().count(), TITLE_WIDTH);
        assert!(cut.ends_with("..."));
    }
}
