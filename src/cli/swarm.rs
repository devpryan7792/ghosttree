//! `gt swarm` (SPEC §5.1/§5.2): dispatch the swarm to the running daemon
//! (auto-starting it), then stream the SPEC §5.2 live dashboard until the
//! arbitrator picks a winner (or the user detaches with 'q').

use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

use crate::cli::open_store;
use crate::daemon::{self, client::RpcClient, DaemonOptions};
use crate::db::short_id;
use crate::ui::dashboard::{self, CandidateRow, DashboardState, Session, TERMINAL_TASK_STATUSES};

/// Per-frame read timeout: also the key-poll cadence for the TUI and the
/// elapsed-clock refresh cadence for the plain-stream mode.
const FRAME_POLL: Duration = Duration::from_millis(100);
/// Re-request the final snapshot this often while the arbitrator settles.
const FINAL_RETRY: Duration = Duration::from_millis(250);
/// Give up waiting for the arbitrator after the last candidate settles.
const FINAL_SETTLE: Duration = Duration::from_secs(15);
/// Hard ceiling for the whole watch loop (idle watchdogs bound candidate
/// lifetimes; this only catches a wedged IPC link).
const OVERALL_DEADLINE: Duration = Duration::from_secs(60 * 30);

/// (variant name, agent command) pairs for the request.
fn variant_plan(n: usize, agents: &str) -> Result<Vec<(String, String)>> {
    let commands: Vec<String> = agents
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect();
    if commands.len() != n {
        bail!(
            "--agents lists {} command(s) but -n {n} variants requested",
            commands.len()
        );
    }
    Ok((0..n)
        .map(|i| (format!("c{}", i + 1), commands[i].clone()))
        .collect())
}

/// Ensure a daemon is supervising this repo: an alive pid wins, otherwise
/// the background daemon is auto-started and awaited to readiness.
fn ensure_daemon(opts: &DaemonOptions) -> Result<()> {
    if let Some(pid) = daemon::read_pid(&opts.pid_path()).ok().flatten() {
        if daemon::pid_alive(pid) {
            if !opts.socket_path().exists() {
                bail!(
                    "daemon (pid {pid}) is running but its socket {} is missing; \
                     run `gt daemon stop` and retry",
                    opts.socket_path().display()
                );
            }
            return Ok(());
        }
    }
    let pid = daemon::start_background(opts).context("failed to auto-start the daemon")?;
    println!("[GhostTree] daemon auto-started (pid {pid})");
    Ok(())
}

/// Build candidate rows from the `SpawnSwarm` result.
fn rows_from_spawn(spawn: &Value) -> Result<Vec<CandidateRow>> {
    let candidates = spawn["candidates"]
        .as_array()
        .context("SpawnSwarm result missing candidates")?;
    let now = Instant::now();
    candidates
        .iter()
        .map(|c| {
            Ok(CandidateRow {
                id: c["id"].as_str().context("candidate id")?.to_string(),
                variant: c["variant_name"]
                    .as_str()
                    .context("variant name")?
                    .to_string(),
                agent: c["agent_cmd"]
                    .as_str()
                    .unwrap_or("")
                    .split_whitespace()
                    .next()
                    .unwrap_or("")
                    .to_string(),
                status: c["status"].as_str().unwrap_or("INITIALIZING").to_string(),
                head_commit: None,
                exit_code: None,
                score: None,
                tests: None,
                heads_seen: Vec::new(),
                first_seen: now,
            })
        })
        .collect()
}

/// Print the §5.2-style spawn banner.
fn print_header(spawn: &Value) {
    let task_id = spawn["task_id"].as_str().unwrap_or("?");
    let base = spawn["base_commit"].as_str().unwrap_or("?");
    let branch = spawn["base_branch"].as_str().unwrap_or("?");
    println!(
        "[GhostTree] Task initialized: {} (Base: {} on {branch})",
        short_id(task_id),
        base.chars().take(7).collect::<String>()
    );
    for c in spawn["candidates"].as_array().unwrap_or(&Vec::new()) {
        println!(
            "  \u{251C}\u{2500}\u{2500} Candidate {} ({}) -> {}",
            short_id(c["id"].as_str().unwrap_or("?")),
            c["variant_name"].as_str().unwrap_or("?"),
            c["worktree_path"].as_str().unwrap_or("?")
        );
    }
    println!("[GhostTree] Spawning workers in pseudo-terminals...");
}

/// Run the whole command.
pub(crate) fn run(
    prompt: String,
    variants: usize,
    agents: String,
    test_cmd: Option<String>,
    timeout_secs: Option<u64>,
    no_ui: bool,
) -> Result<()> {
    if variants == 0 {
        bail!("-n requires at least 1 variant");
    }
    // Validate the swarm shape before touching the daemon at all.
    let plans = variant_plan(variants, &agents)?;
    let (repo, _store) = open_store()?;
    let opts = DaemonOptions::new(repo.root().to_path_buf());
    ensure_daemon(&opts)?;

    let mut client = RpcClient::connect(&opts.socket_path()).context("daemon socket connect")?;
    let mut params = json!({
        "prompt": prompt,
        "variants": plans
            .iter()
            .map(|(name, cmd)| json!({"name": name, "agent_cmd": cmd}))
            .collect::<Vec<_>>(),
    });
    if let Some(cmd) = &test_cmd {
        params["test_cmd"] = json!(cmd);
    }
    if let Some(secs) = timeout_secs {
        params["timeout_secs"] = json!(secs);
    }

    let spawn_id = client.next_id();
    let spawn = client
        .call(spawn_id, "SpawnSwarm", params)
        .context("SpawnSwarm dispatch failed")?;

    print_header(&spawn);

    let mut state = DashboardState::new(spawn["task_id"].as_str().unwrap_or(""), Instant::now());
    for row in rows_from_spawn(&spawn)? {
        state.add_row(row);
    }
    // Transitions that raced in before the spawn response was read.
    for note in client.take_notifications() {
        if note["method"].as_str() == Some("CandidateStateChanged") {
            state.apply_state_change(&note["params"]);
        }
    }

    let interactive = !no_ui && dashboard::interactive_available();
    let mut session = if interactive {
        Some(Session::enter().context("failed to enter the interactive dashboard")?)
    } else {
        None
    };

    let outcome = watch_loop(&mut client, &mut state, &mut session);
    // Restore the console before printing anything new.
    if let Some(session) = session.as_mut() {
        let _ = session.leave();
    }
    outcome?;

    for line in state.summary_lines() {
        println!("{line}");
    }
    Ok(())
}

/// Frame-processing loop shared by both render modes.
///
/// Ends when the task reports a terminal status, when arbitration stalls
/// (`FINAL_SETTLE` after the last candidate settles), on 'q' (interactive),
/// on the hard deadline, or on connection loss.
fn watch_loop(
    client: &mut RpcClient,
    state: &mut DashboardState,
    interactive: &mut Option<Session>,
) -> Result<()> {
    // Frame reads return empty on this poll window so the loop can tick the
    // elapsed clock, poll for 'q', and chase the final arbitration snapshot.
    client
        .set_read_timeout(Some(FRAME_POLL))
        .context("socket read timeout")?;
    let deadline = Instant::now() + OVERALL_DEADLINE;
    let mut pending_final: Option<i64> = None;
    let mut final_first_sent: Option<Instant> = None;
    let mut last_final_sent = Instant::now();

    loop {
        if Instant::now() >= deadline {
            bail!("gave up watching the swarm after {OVERALL_DEADLINE:?}");
        }

        if let Some(session) = interactive {
            match session.poll_quit(FRAME_POLL) {
                Ok(true) => return Ok(()),
                Ok(false) => {}
                Err(e) => bail!("terminal input failed: {e}"),
            }
        }

        match client.next_frame() {
            Ok(Some(frame)) => {
                if frame["method"].as_str() == Some("CandidateStateChanged") {
                    if let Some(index) = state.apply_state_change(&frame["params"]) {
                        emit_change(state, interactive, index);
                    }
                } else if let (Some(pending), Some(id)) =
                    (pending_final, frame.get("id").and_then(Value::as_i64))
                {
                    if id == pending {
                        if let Some(task) = frame.get("result").and_then(|r| r.get("task")) {
                            state.apply_snapshot(task);
                        }
                        pending_final = None;
                    }
                }
                if terminal_now(state) {
                    return Ok(());
                }
            }
            Ok(None) => {}
            Err(e) => return Err(e).context("daemon connection failed while watching the swarm"),
        }

        if !state.all_settled() {
            if let Some(session) = interactive {
                let _ = session.render(state);
            }
            continue;
        }

        // Everything settled: chase the task snapshot (with winner/score
        // data) until it goes terminal or the arbitrator visibly stalls.
        let first_sent = *final_first_sent.get_or_insert(Instant::now());
        if first_sent.elapsed() > FINAL_SETTLE {
            return Ok(());
        }
        if pending_final.is_none() && last_final_sent.elapsed() > FINAL_RETRY {
            let id = client.next_id();
            client
                .send(&json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "method": "GetStatus",
                    "params": {"task_id": state.task_id, "all": true},
                }))
                .context("final GetStatus dispatch failed")?;
            pending_final = Some(id);
            last_final_sent = Instant::now();
        } else if pending_final.is_none() {
            if let Some(session) = interactive {
                let _ = session.render(state);
            }
        }
    }
}

/// Interactive repaint or (in plain mode) one progress line per transition.
fn emit_change(state: &DashboardState, interactive: &mut Option<Session>, index: usize) {
    match interactive {
        Some(session) => {
            let _ = session.render(state);
        }
        None => println!("{}", dashboard::transition_line(state, index)),
    }
}

/// Terminal state reached: a task-level terminal status from the wire.
fn terminal_now(state: &DashboardState) -> bool {
    state
        .task_status
        .as_deref()
        .is_some_and(|status| TERMINAL_TASK_STATUSES.contains(&status))
}
