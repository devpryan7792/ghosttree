//! Live swarm dashboard (SPEC §5.2 "[Live Dashboard - Press 'q' to exit]").
//!
//! The state model is decoded straight from the daemon's wire shapes
//! (`CandidateStateChanged` notifications, `GetStatus` snapshots) so the CLI
//! loop stays thin. Rendering is crossterm into any `io::Write`, which keeps
//! the table format assertable in unit tests.

use std::io::{self, Stdout, Write};
use std::time::{Duration, Instant};

use crossterm::cursor::{Hide, MoveTo, Show};
use crossterm::event::{self, Event, KeyCode, KeyEvent};
use crossterm::execute;
use crossterm::terminal::{Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::{queue, tty::IsTty};
use serde_json::Value;

use crate::db::short_id;

/// Statuses after which a candidate can never change again (SPEC §3.2).
pub const TERMINAL_CANDIDATE_STATUSES: [&str; 3] = ["READY", "FAILED", "MERGED"];
/// Task statuses that end the dashboard loop.
pub const TERMINAL_TASK_STATUSES: [&str; 2] = ["COMPLETED", "ABORTED"];

/// One candidate's live row.
#[derive(Debug, Clone, PartialEq)]
pub struct CandidateRow {
    pub id: String,
    pub variant: String,
    /// Agent as displayed in the table: first token of the agent command.
    pub agent: String,
    pub status: String,
    pub head_commit: Option<String>,
    pub exit_code: Option<i32>,
    pub score: Option<f64>,
    /// Stored test results as `(passed, failed)`.
    pub tests: Option<(u32, u32)>,
    /// Distinct commit heads observed — the COMMITS column.
    pub heads_seen: Vec<String>,
    pub first_seen: Instant,
}

/// Selected winner rendered by the dashboard.
#[derive(Debug, Clone, PartialEq)]
pub struct Winner {
    pub candidate_id: String,
    pub variant: String,
    pub score: f64,
}

/// Accumulated live state for one running swarm.
#[derive(Debug, Clone)]
pub struct DashboardState {
    pub task_id: String,
    pub title: Option<String>,
    pub rows: Vec<CandidateRow>,
    pub started_at: Instant,
    pub task_status: Option<String>,
    pub winner: Option<Winner>,
}

impl DashboardState {
    /// Empty dashboard; rows arrive with the spawn response and later
    /// notifications.
    pub fn new(task_id: impl Into<String>, started_at: Instant) -> Self {
        Self {
            task_id: task_id.into(),
            title: None,
            rows: Vec::new(),
            started_at,
            task_status: None,
            winner: None,
        }
    }

    pub fn add_row(&mut self, row: CandidateRow) {
        self.rows.push(row);
    }

    /// Mutable row lookup by full candidate id.
    pub fn row_mut(&mut self, candidate_id: &str) -> Option<&mut CandidateRow> {
        self.rows.iter_mut().find(|row| row.id == candidate_id)
    }

    /// Apply a `CandidateStateChanged` params object. `Some(row_index)` is
    /// the changed row (callers render/print it); `None` means "no usable
    /// row for this notification".
    pub fn apply_state_change(&mut self, params: &Value) -> Option<usize> {
        let candidate_id = params.get("candidate_id").and_then(Value::as_str)?;
        let status = params.get("status").and_then(Value::as_str)?;
        let row = self.row_mut(candidate_id)?;
        row.status = status.to_string();
        if let Some(head) = params.get("head_commit").and_then(Value::as_str) {
            if !head.is_empty() && !row.heads_seen.iter().any(|seen| seen == head) {
                row.heads_seen.push(head.to_string());
                row.head_commit = Some(head.to_string());
            }
        }
        self.rows.iter().position(|row| row.id == candidate_id)
    }

    /// Sync every column from a `GetStatus` task snapshot. The arbitrator
    /// may pick the winner here: scores/test results/statuses all land.
    pub fn apply_snapshot(&mut self, task_view: &Value) -> bool {
        let mut changed = false;
        if let Some(status) = task_view.get("status").and_then(Value::as_str) {
            if self.task_status.as_deref() != Some(status) {
                self.task_status = Some(status.to_string());
                changed = true;
            }
        }
        if self.title.is_none() {
            if let Some(title) = task_view.get("title").and_then(Value::as_str) {
                self.title = Some(title.to_string());
            }
        }
        if let Some(candidates) = task_view.get("candidates").and_then(Value::as_array) {
            for view in candidates {
                changed |= self.apply_candidate_view(view);
            }
        }
        // Winner: needs the variant name + score from the row it points at.
        if let Some(TaskWinnerProbe {
            candidate_id,
            score,
        }) = task_winner_probe(task_view)
        {
            let variant = self
                .rows
                .iter()
                .find(|row| row.id == candidate_id)
                .map(|row| row.variant.clone());
            if let Some(variant) = variant {
                let next = Winner {
                    candidate_id: candidate_id.clone(),
                    variant,
                    score,
                };
                if self.winner.as_ref() != Some(&next) {
                    self.winner = Some(next);
                    changed = true;
                }
            }
        }
        changed
    }

    /// Merge one `CandidateView` object into its row.
    fn apply_candidate_view(&mut self, view: &Value) -> bool {
        let Some(candidate_id) = view.get("id").and_then(Value::as_str) else {
            return false;
        };
        let Some(row) = self.row_mut(candidate_id) else {
            return false;
        };
        let mut changed = false;
        if let Some(status) = view.get("status").and_then(Value::as_str) {
            if row.status != status {
                row.status = status.to_string();
                changed = true;
            }
        }
        if let Some(score) = view.get("score").and_then(Value::as_f64) {
            // 0.0 is the schema default for unscored candidates; treat it as
            // "no score yet" so the dashboard keeps its "--" placeholder.
            let next = if score == 0.0 && row.status != "MERGED" && row.tests.is_none() {
                None
            } else {
                Some(score)
            };
            if row.score != next {
                row.score = next;
                changed = true;
            }
        }
        if let Some(results) = view.get("test_results") {
            if !results.is_null() {
                let pair = Some((
                    results.get("passed").and_then(Value::as_u64).unwrap_or(0) as u32,
                    results.get("failed").and_then(Value::as_u64).unwrap_or(0) as u32,
                ));
                if row.tests != pair {
                    row.tests = pair;
                    changed = true;
                }
            }
        }
        if let Some(head) = view.get("head_commit").and_then(Value::as_str) {
            if !head.is_empty() && !row.heads_seen.iter().any(|seen| seen == head) {
                row.heads_seen.push(head.to_string());
                row.head_commit = Some(head.to_string());
                changed = true;
            }
        }
        changed
    }

    /// True once every row reached a terminal candidate status (the loop
    /// then asks for the final snapshot to learn the winner).
    pub fn all_settled(&self) -> bool {
        !self.rows.is_empty()
            && self
                .rows
                .iter()
                .all(|row| TERMINAL_CANDIDATE_STATUSES.contains(&row.status.as_str()))
    }

    /// Final summary lines to print after (or without) the TUI.
    pub fn summary_lines(&self) -> Vec<String> {
        let mut lines = Vec::new();
        let status = self.task_status.clone().unwrap_or_else(|| "RUNNING".into());
        match &self.winner {
            Some(winner) => {
                lines.push(format!(
                    "[GhostTree] Winner detected: {} ({}, score {:.1}) — task {status}.",
                    short_id(&winner.candidate_id),
                    winner.variant,
                    winner.score
                ));
                lines.push(format!(
                    "Run 'gt merge {}' to promote changes into the active branch.",
                    short_id(&winner.candidate_id)
                ));
            }
            None if TERMINAL_TASK_STATUSES.contains(&status.as_str())
                || (status == "EVALUATING" && self.all_settled()) =>
            {
                lines.push(format!(
                    "[GhostTree] Task {status} with no promotable candidate. \
                     Details: gt list / gt eval --re-run {}",
                    short_id(&self.task_id)
                ))
            }
            None => {
                // Still running (user detached or swarm incomplete).
                lines.push(format!(
                    "[GhostTree] Task {} still {status}. Detached; check 'gt list'.",
                    short_id(&self.task_id)
                ));
            }
        }
        lines
    }

    /// Plain-text table for unit tests (the ANSI renderer is asserted with
    /// content markers instead).
    pub fn plain_table(&self, now: Instant) -> String {
        let mut out = String::new();
        out.push_str(&format!(
            "{:<7}  {:<14}  {:<11}  {:>8}  {:>7}  {:>6}  {:>6}\n",
            "ID", "AGENT", "STATUS", "ELAPSED", "COMMITS", "TESTS", "SCORE"
        ));
        out.push_str(&format!("{}\n", "-".repeat(74)));
        for row in &self.rows {
            out.push_str(&format!(
                "{:<7}  {:<14}  {:<11}  {:>8}  {:>7}  {:>6}  {:>6}\n",
                short_id(&row.id),
                abbreviate_agent(&row.agent, 14),
                row.status,
                format_elapsed(now.duration_since(self.started_at)),
                row.heads_seen.len(),
                tests_cell(row),
                score_cell(row),
            ));
        }
        out
    }
}

/// `winner_candidate_id` + that candidate's score from a task snapshot.
struct TaskWinnerProbe {
    candidate_id: String,
    score: f64,
}

fn task_winner_probe(task_view: &Value) -> Option<TaskWinnerProbe> {
    let candidate_id = task_view
        .get("winner_candidate_id")
        .and_then(Value::as_str)?
        .to_string();
    let score = task_view
        .get("candidates")
        .and_then(Value::as_array)?
        .iter()
        .find(|c| c.get("id").and_then(Value::as_str) == Some(candidate_id.as_str()))
        .and_then(|c| c.get("score"))
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    Some(TaskWinnerProbe {
        candidate_id,
        score,
    })
}

/// First whitespace token of an agent command, truncated for the table.
pub fn abbreviate_agent(agent: &str, max: usize) -> String {
    let first = agent.split_whitespace().next().unwrap_or(agent);
    if first.chars().count() > max {
        let cut: String = first.chars().take(max - 1).collect();
        format!("{cut}\u{2026}")
    } else {
        first.to_string()
    }
}

/// `01m 24s` style elapsed display.
pub fn format_elapsed(d: Duration) -> String {
    let secs = d.as_secs();
    format!("{:02}m {:02}s", secs / 60, secs % 60)
}

/// `8/8`, `3/8`, or `--`.
fn tests_cell(row: &CandidateRow) -> String {
    match row.tests {
        Some((passed, failed)) => format!("{}/{}", passed, passed + failed),
        None => "--".to_string(),
    }
}

/// Score formatted to one decimal (also `-0.0` normalization).
fn score_cell(row: &CandidateRow) -> String {
    match row.score {
        Some(score) => format!("{:.1}", if score == 0.0 { 0.0 } else { score }),
        None => "--".to_string(),
    }
}

// ---------------------------------------------------------------------------
// Interactive terminal session.
// ---------------------------------------------------------------------------

/// Owns the raw-mode/alternate-screen lifecyle of the interactive dashboard.
/// Dropping the session restores the terminal even on early returns.
pub struct Session {
    stdout: Stdout,
    closed: bool,
}

impl Session {
    /// Switch to raw mode on the alternate screen (no echo, full redraw).
    pub fn enter() -> io::Result<Self> {
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen, Hide)?;
        crossterm::terminal::enable_raw_mode()?;
        Ok(Self {
            stdout,
            closed: false,
        })
    }

    /// Restore the terminal (always safe to call twice).
    pub fn leave(&mut self) -> io::Result<()> {
        if self.closed {
            return Ok(());
        }
        self.closed = true;
        let result = execute!(
            &mut self.stdout,
            Show,
            Clear(ClearType::FromCursorDown),
            LeaveAlternateScreen
        );
        crossterm::terminal::disable_raw_mode()?;
        result
    }

    /// Repaint the whole dashboard.
    pub fn render(&mut self, state: &DashboardState) -> io::Result<()> {
        render_to(&mut self.stdout, state)
    }

    /// Poll the keyboard for a quit gesture ('q', Esc, Ctrl-C) within
    /// `timeout`; other events are swallowed.
    pub fn poll_quit(&mut self, timeout: Duration) -> io::Result<bool> {
        if !event::poll(timeout)? {
            return Ok(false);
        }
        match event::read()? {
            Event::Key(KeyEvent {
                code, modifiers, ..
            }) => Ok(match code {
                KeyCode::Char('q') | KeyCode::Esc => true,
                KeyCode::Char('c') => modifiers.contains(event::KeyModifiers::CONTROL),
                _ => false,
            }),
            _ => Ok(false),
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.leave();
    }
}

/// Draw the dashboard onto any writer (ANSI cursor/control sequences).
pub fn render_to<W: Write>(out: &mut W, state: &DashboardState) -> io::Result<()> {
    let now = Instant::now();
    queue!(out, MoveTo(0, 0), Clear(ClearType::All))?;

    match &state.title {
        Some(title) => writeln!(
            out,
            "[GhostTree] {} — live dashboard (press 'q' to detach)  [elapsed {}]",
            title,
            format_elapsed(now.duration_since(state.started_at))
        )?,
        None => writeln!(
            out,
            "[GhostTree] task {} — live dashboard (press 'q' to detach)",
            short_id(&state.task_id)
        )?,
    }
    writeln!(out)?;

    let rows: Vec<(String, String, String, String, usize, String, String)> = state
        .rows
        .iter()
        .map(|row| {
            (
                short_id(&row.id),
                abbreviate_agent(&row.agent, 14),
                row.status.clone(),
                format_elapsed(now.duration_since(row.first_seen)),
                row.heads_seen.len(),
                tests_cell(row),
                score_cell(row),
            )
        })
        .collect();

    let id_w = rows.iter().map(|r| r.0.len()).max().unwrap_or(2).max(2);
    let agent_w = rows
        .iter()
        .map(|r| r.1.chars().count())
        .max()
        .unwrap_or(5)
        .max("AGENT".len());
    let status_w = rows
        .iter()
        .map(|r| r.2.len())
        .max()
        .unwrap_or(6)
        .max("STATUS".len());

    // Header
    writeln!(
        out,
        "  {:<id_w$}  {:<agent_w$}  {:<status_w$}  {:>8}  {:>7}  {:>6}  {:>6}",
        "ID", "AGENT", "STATUS", "ELAPSED", "COMMITS", "TESTS", "SCORE",
    )?;
    writeln!(out, "  {}", "-".repeat(id_w + agent_w + status_w + 44))?;
    for (id, agent, status, elapsed, commits, tests, score) in &rows {
        writeln!(
            out,
            "  {:<id_w$}  {:<agent_w$}  {:<status_w$}  {:>8}  {:>7}  {:>6}  {:>6}",
            id, agent, status, elapsed, commits, tests, score,
        )?;
    }
    out.flush()
}

/// Header line for a state transition in non-interactive mode.
pub fn transition_line(state: &DashboardState, row_index: usize) -> String {
    let row = &state.rows[row_index];
    format!(
        "[GhostTree] candidate {} ({}) -> {}{}",
        row.variant,
        short_id(&row.id),
        row.status,
        row.head_commit
            .as_deref()
            .map(|head| format!(" @{}", &head[..head.len().min(7)]))
            .unwrap_or_default(),
    )
}

/// Winner banner (plain text; the summary contract is asserted in tests).
pub fn winner_banner(state: &DashboardState) -> String {
    match &state.winner {
        Some(winner) => format!(
            "[GhostTree] Winner: {} / {} (score {:.1})",
            winner.variant,
            short_id(&winner.candidate_id),
            winner.score
        ),
        None => "[GhostTree] No winner selected.".to_string(),
    }
}

/// Is a full-screen dashboard usable here? (Tests and pipes get the plain
/// streaming mode automatically.)
pub fn interactive_available() -> bool {
    io::stdout().is_tty()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn started_now() -> Instant {
        Instant::now()
    }

    fn row(id: &str, variant: &str, agent: &str, status: &str) -> CandidateRow {
        CandidateRow {
            id: id.to_string(),
            variant: variant.to_string(),
            agent: agent.to_string(),
            status: status.to_string(),
            head_commit: None,
            exit_code: None,
            score: None,
            tests: None,
            heads_seen: Vec::new(),
            first_seen: started_now(),
        }
    }

    #[test]
    fn state_change_updates_status_and_counts_commits() {
        let start = started_now();
        let mut state = DashboardState::new("task-1", start);
        state.add_row(row("c-full-1", "c1", "echo ok", "INITIALIZING"));
        state.add_row(row("c-full-2", "c2", "echo bad", "INITIALIZING"));

        let changed = state.apply_state_change(&json!({
            "candidate_id": "c-full-1",
            "status": "WORKING",
            "head_commit": "e4f810aaaabb",
            "timestamp": 1729245100120i64
        }));
        assert_eq!(changed, Some(0));
        let row = state.row_mut("c-full-1").unwrap();
        assert_eq!(row.status, "WORKING");
        assert_eq!(row.heads_seen.len(), 1);

        // Same head twice is a no-op for the commit counter.
        state.apply_state_change(&json!({
            "candidate_id": "c-full-1",
            "status": "WORKING",
            "head_commit": "e4f810aaaabb"
        }));
        assert_eq!(state.row_mut("c-full-1").unwrap().heads_seen.len(), 1);

        state.apply_state_change(&json!({
            "candidate_id": "c-full-1",
            "status": "WORKING",
            "head_commit": "aa11bb22cc33"
        }));
        assert_eq!(state.row_mut("c-full-1").unwrap().heads_seen.len(), 2);

        // Unknown candidates are ignored cleanly.
        assert_eq!(
            state.apply_state_change(&json!({
                "candidate_id": "ghost",
                "status": "READY"
            })),
            None
        );
        assert_eq!(
            state.apply_state_change(&json!({ "status": "READY" })),
            None
        );
    }

    #[test]
    fn snapshot_fills_scores_tests_and_winner() {
        let start = started_now();
        let mut state = DashboardState::new("task-1", start);
        state.add_row(row("c-full-1", "c1", "fixer", "READY"));
        state.add_row(row("c-full-2", "c2", "hallucinator", "FAILED"));

        let task = json!({
            "id": "task-1",
            "title": "Fix race condition",
            "status": "COMPLETED",
            "winner_candidate_id": "c-full-1",
            "candidates": [
                {
                    "id": "c-full-1", "variant_name": "c1", "status": "READY",
                    "score": 96.4, "exit_code": 0,
                    "test_results": {"passed": 8, "failed": 0, "duration_ms": 420}
                },
                {
                    "id": "c-full-2", "variant_name": "c2", "status": "FAILED",
                    "score": 32.0, "exit_code": 1,
                    "test_results": {"passed": 3, "failed": 5, "duration_ms": 100}
                }
            ]
        });
        assert!(state.apply_snapshot(&task));
        assert_eq!(state.task_status.as_deref(), Some("COMPLETED"));
        assert_eq!(state.title.as_deref(), Some("Fix race condition"));
        let winner = state.winner.clone().expect("winner decoded");
        assert_eq!(winner.candidate_id, "c-full-1");
        assert_eq!(winner.variant, "c1");
        assert!((winner.score - 96.4).abs() < 1e-9);
        let row = state.row_mut("c-full-1").unwrap();
        assert_eq!(row.tests, Some((8, 0)));
        assert_eq!(tests_cell(row), "8/8");

        // Re-applying the same snapshot is not a change (no re-render churn).
        let mut copy = state.clone();
        assert!(!copy.apply_snapshot(&task));
    }

    #[test]
    fn all_settled_matches_terminal_rows() {
        let start = started_now();
        let mut state = DashboardState::new("task-1", start);
        state.add_row(row("c-full-1", "c1", "echo", "READY"));
        assert!(state.all_settled());

        let mut running = DashboardState::new("task-2", start);
        running.add_row(row("c-full-1", "c1", "echo", "TESTING"));
        assert!(!running.all_settled());

        let empty = DashboardState::new("task-3", start);
        assert!(!empty.all_settled(), "no rows means nothing settled");
    }

    #[test]
    fn render_writes_headers_rows_and_wins() {
        let start = Instant::now() - Duration::from_secs(84);
        let mut state = DashboardState::new("task-1", start);
        state.title = Some("Fix the race".to_string());
        state.add_row(row("c-full-aaaa", "c1", "claude-code --run", "TESTING"));
        state.add_row(row("c-full-bbbb", "c2", "aider --yes", "FAILED"));
        let row = state.row_mut("c-full-aaaa").unwrap();
        row.heads_seen = vec!["a".into(), "b".into()];
        row.tests = Some((8, 0));
        row.score = Some(96.4);

        let mut buffer: Vec<u8> = Vec::new();
        render_to(&mut buffer, &state).expect("render");
        let text = String::from_utf8(buffer).unwrap();
        // The spec's §5.2 columns all appear.
        for column in [
            "ID", "AGENT", "STATUS", "ELAPSED", "COMMITS", "TESTS", "SCORE",
        ] {
            assert!(text.contains(column), "missing {column} in {text:?}");
        }
        assert!(text.contains("Fix the race"));
        assert!(text.contains("claude-code"), "{text:?}");
        assert!(text.contains("TESTING"));
        assert!(text.contains("8/8"));
        assert!(text.contains("96.4"));
        // Elapsed is rendered at render time; the 84s start diff shows up.
        assert!(text.contains("01m"), "{text:?}");
    }

    #[test]
    fn plain_table_matches_column_contract() {
        let start = Instant::now() - Duration::from_secs(62);
        let mut state = DashboardState::new("task-1", start);
        state.add_row(row("c-full-1", "c1", "echo", "READY"));
        let table = state.plain_table(Instant::now());
        assert!(table.contains("01m"), "{table}");
        assert!(table.contains("READY"));
        assert!(table.contains("--"), "unscored candidates show --");
    }

    #[test]
    fn summary_lines_cover_winner_and_no_winner_paths() {
        let start = started_now();
        let mut state = DashboardState::new("01JMW8KXYZ", start);
        state.task_status = Some("COMPLETED".into());
        state.winner = Some(Winner {
            candidate_id: "c-full-1234567".to_string(),
            variant: "c1".into(),
            score: 96.4,
        });
        let lines = state.summary_lines();
        assert!(lines[0].contains("Winner detected"), "{lines:?}");
        // id is cut to its 7-char short form in the hint line.
        assert!(lines[1].contains("gt merge c-full-"), "{lines:?}");

        let mut no_winner = DashboardState::new("task", start);
        no_winner.task_status = Some("EVALUATING".into());
        // Settled rows + EVALUATING == the arbitrator found nothing promotable.
        no_winner.add_row(row("c-full-1", "c1", "echo", "FAILED"));
        let lines = no_winner.summary_lines();
        assert!(lines[0].contains("no promotable"), "{lines:?}");

        let mut aborted = DashboardState::new("task", start);
        aborted.task_status = Some("ABORTED".into());
        assert!(aborted.summary_lines()[0].contains("ABORTED"));
    }

    #[test]
    fn elapsed_and_agent_formatting() {
        assert_eq!(format_elapsed(Duration::from_secs(84)), "01m 24s");
        assert_eq!(format_elapsed(Duration::from_secs(9)), "00m 09s");
        assert_eq!(abbreviate_agent("claude-code --flag", 14), "claude-code");
        assert_eq!(abbreviate_agent("aider", 14), "aider");
        let cut = abbreviate_agent("verylongagentcommand--with-flags", 10);
        assert_eq!(cut.chars().count(), 10, "{cut}");
        assert!(cut.ends_with('\u{2026}'));
    }

    #[test]
    fn transition_line_reports_variant_and_status() {
        let start = started_now();
        let mut state = DashboardState::new("task", start);
        state.add_row(row("c-full-1", "fast-a", "echo", "WORKING"));
        let idx = state.apply_state_change(&json!({
            "candidate_id": "c-full-1",
            "status": "TESTING",
            "head_commit": "deadbeef123"
        }));
        assert_eq!(idx, Some(0));
        let line = transition_line(&state, idx.unwrap());
        assert!(line.contains("fast-a"), "{line}");
        assert!(line.contains("TESTING"));
        assert!(line.contains("@deadbee"), "{line}");
    }

    #[test]
    fn unscored_zero_with_no_tests_stays_placeholder() {
        let start = started_now();
        let mut state = DashboardState::new("task", start);
        state.add_row(row("c-full-1", "c1", "echo", "READY"));
        state.apply_snapshot(&json!({
            "status": "COMPLETED",
            "candidates": [{
                "id": "c-full-1", "status": "READY", "score": 0.0
            }]
        }));
        let row = state.row_mut("c-full-1").unwrap();
        assert_eq!(
            row.score, None,
            "schema-default 0.0 is not a computed score"
        );
        // But an explicitly scored zero after a test run is a real zero.
        row.tests = Some((0, 0));
        let mut state2 = state.clone();
        state2.apply_snapshot(&json!({
            "status": "COMPLETED",
            "candidates": [{
                "id": "c-full-1", "status": "READY", "score": 0.0,
                "test_results": {"passed": 0, "failed": 0, "duration_ms": 0}
            }]
        }));
        assert_eq!(state2.row_mut("c-full-1").unwrap().score, Some(0.0));
    }
}
