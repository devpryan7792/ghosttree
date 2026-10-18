//! Daemon IPC integration tests (SPEC §6.2 JSON-RPC over UDS).
//!
//! Harness: the daemon core runs in-process (`serve_with_shutdown`) against a
//! hermetic temp repo so transitions/persistence are assertable; lifecycle
//! behavior through the real `gt` binary is covered at the end.

mod common;

use common::{git, TestRepo};

use ghosttree::daemon::{self, DaemonOptions};
use ghosttree::db::{CandidateStatus, EventType, NewCandidate, NewTask, Store, TaskStatus};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::Command as StdCommand;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

// The integration tests need a small, inspectable handle: shutdown + state.
struct TestDaemon {
    flag: Arc<AtomicBool>,
    socket: PathBuf,
    handle: Mutex<Option<JoinHandle<std::io::Result<()>>>>,
}

impl TestDaemon {
    /// Start serving on a fresh thread and wait until the socket is bound.
    fn start(root: &Path) -> Self {
        Self::start_opts(DaemonOptions::new(root.to_path_buf()))
    }

    fn start_opts(opts: DaemonOptions) -> Self {
        let socket = opts.socket_path();
        let flag = Arc::new(AtomicBool::new(false));
        let serve_flag = flag.clone();
        let serve_opts = opts.clone();
        let handle = thread::spawn(move || daemon::serve_with_shutdown(&serve_opts, serve_flag));
        let deadline = Instant::now() + Duration::from_secs(5);
        while !socket.exists() {
            assert!(
                Instant::now() < deadline,
                "daemon socket did not appear within 5s at {}",
                socket.display()
            );
            thread::sleep(Duration::from_millis(20));
        }
        // Bind may exist before serve's pid file is written; give it a beat.
        thread::sleep(Duration::from_millis(50));

        Self {
            flag,
            socket,
            handle: Mutex::new(Some(handle)),
        }
    }

    /// Flip the shutdown flag and wait for the socket file to vanish.
    fn stop(self) {
        self.flag.store(true, Ordering::SeqCst);
        let deadline = Instant::now() + Duration::from_secs(5);
        while self.socket.exists() {
            assert!(
                Instant::now() < deadline,
                "daemon did not release its socket within 5s"
            );
            thread::sleep(Duration::from_millis(20));
        }
        if let Some(handle) = self.handle.lock().expect("daemon handle").take() {
            let _ = handle.join();
        }
    }
}

/// Minimal newline-delimited JSON-RPC client for assertions.
struct Client {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
    /// Notifications that arrived while waiting for responses (used by
    /// wait_state because broadcasts can precede the spawning response).
    seen: Vec<Value>,
    /// Monotonic id counter for wait helpers that issue their own requests.
    next_id: Option<i64>,
}

impl Client {
    fn connect(socket: &PathBuf) -> Self {
        let stream = UnixStream::connect(socket).expect("daemon socket connect");
        let _ = stream.set_read_timeout(Some(Duration::from_secs(60)));
        let writer = stream.try_clone().expect("clone rpc socket");
        Self {
            reader: BufReader::new(stream),
            writer,
            seen: Vec::new(),
            next_id: None,
        }
    }

    fn send_raw_line(&mut self, line: &str) {
        writeln!(self.writer, "{line}").expect("write rpc frame");
    }

    /// Read the next frame (response or notification). Panics on EOF/timeout.
    fn read_frame(&mut self) -> Value {
        let mut line = String::new();
        let n = self
            .reader
            .read_line(&mut line)
            .expect("read rpc frame (peer closed or stalled)");
        assert!(n > 0, "daemon closed the connection unexpectedly");
        serde_json::from_str(line.trim())
            .unwrap_or_else(|e| panic!("daemon sent non-JSON frame {line:?}: {e}"))
    }

    /// Send a request and block for ITS response, buffering notifications.
    fn request(&mut self, id: i64, method: &str, params: Value) -> Value {
        self.send_raw_line(
            &json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}).to_string(),
        );
        loop {
            let frame = self.read_frame();
            if is_response_for(&frame, id) {
                return frame;
            }
            if frame.get("method").is_some() {
                self.seen.push(frame);
            }
        }
    }

    /// Pull a buffered state change for `candidate_id` == `want`, if any.
    fn take_state(&mut self, candidate_id: &str, want: &str) -> Option<Value> {
        let position = self
            .seen
            .iter()
            .position(|frame| state_change_for(frame, candidate_id).as_deref() == Some(want));
        position.map(|i| self.seen.remove(i))
    }

    /// Wait for a CandidateStateChanged notification for the candidate
    /// reporting `want` (fresh stream first, otherwise buffered history).
    fn wait_state(&mut self, candidate_id: &str, want: &str, timeout: Duration) -> Value {
        if let Some(frame) = self.take_state(candidate_id, want) {
            return frame;
        }
        let deadline = Instant::now() + timeout;
        loop {
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {candidate_id} -> {want}"
            );
            let frame = self.read_frame();
            if state_change_for(&frame, candidate_id).as_deref() == Some(want) {
                return frame;
            }
            if frame.get("method").is_some() {
                self.seen.push(frame);
            }
        }
    }

    /// Poll GetStatus until the task reports `want` (the arbitrator flips
    /// tasks to COMPLETED asynchronously after the last candidate settles).
    fn wait_task_status(&mut self, task_id: &str, want: &str, timeout: Duration) -> Value {
        let deadline = Instant::now() + timeout;
        loop {
            let id = self.next_id();
            let response = self.request(id, "GetStatus", json!({"task_id": task_id}));
            let status = response["result"]["task"]["status"]
                .as_str()
                .unwrap_or("")
                .to_string();
            if status == want {
                return response;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for task {task_id} -> {want} (last: {status})"
            );
            thread::sleep(Duration::from_millis(50));
        }
    }

    fn next_id(&mut self) -> i64 {
        let slot = self.next_id.get_or_insert(500);
        let id = *slot;
        *slot += 1;
        id
    }
}

fn is_response_for(frame: &Value, id: i64) -> bool {
    frame.get("method").is_none() && frame.get("id").and_then(Value::as_i64) == Some(id)
}

/// Extract the reported status when `frame` is a CandidateStateChanged
/// notification for `candidate_id`.
fn state_change_for(frame: &Value, candidate_id: &str) -> Option<String> {
    if frame.get("method").and_then(Value::as_str) == Some("CandidateStateChanged") {
        let params = frame.get("params")?;
        if params.get("candidate_id").and_then(Value::as_str) == Some(candidate_id) {
            return params
                .get("status")
                .and_then(Value::as_str)
                .map(String::from);
        }
    }
    None
}

fn open_store(root: &Path) -> Store {
    Store::open(root.join(".ghosttree").join("ghosttree.db")).expect("open state store")
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[test]
fn rpc_surface_rejects_unknown_methods_and_params() {
    let repo = TestRepo::new();
    let daemon = TestDaemon::start(&repo.root);
    let mut client = Client::connect(&daemon.socket);

    // Empty snapshot over a fresh repo.
    let response = client.request(1, "GetStatus", json!({}));
    assert_eq!(response["result"]["tasks"], json!([]));

    // Unknown method -> -32601.
    let response = client.request(2, "Nope", json!({}));
    assert_eq!(response["error"]["code"], -32601);

    // CandidateStateChanged is notification-only -> -32601 with guidance.
    let response = client.request(3, "CandidateStateChanged", json!({}));
    let message = response["error"]["message"].as_str().expect("message");
    assert!(message.contains("notification"), "{message}");

    // Invalid params on GetStatus (unknown task id) -> -32602.
    let response = client.request(4, "GetStatus", json!({"task_id": "NOPE"}));
    assert_eq!(response["error"]["code"], -32602);

    // Malformed JSON -> -32700 parse error with a null id.
    client.send_raw_line("{not json");
    let mut guard = client;
    let frame = guard.read_frame();
    assert_eq!(frame["error"]["code"], -32700);
    assert!(frame["id"].is_null());
    drop(guard);

    daemon.stop();
}

#[test]
fn spawn_swarm_runs_to_ready_with_broadcast_transitions() {
    let repo = TestRepo::new();
    let daemon = TestDaemon::start(&repo.root);
    let mut client = Client::connect(&daemon.socket);

    let response = client.request(
        1,
        "SpawnSwarm",
        json!({
            "prompt": "make README fancier",
            "variants": [
                {"name": "fast-a", "agent_cmd": "echo working; exit 0"},
                {"name": "fast-b", "agent_cmd": "echo working b; exit 0"}
            ]
        }),
    );
    let result = &response["result"];
    assert_eq!(result["base_branch"], "main");
    let candidates = result["candidates"].as_array().expect("candidates");
    assert_eq!(candidates.len(), 2);
    for c in candidates {
        assert_eq!(c["status"], "WORKING");
        assert!(c["pid"].is_i64(), "expected live pid: {c}");
        assert_eq!(
            c["worktree_path"],
            format!(
                ".ghosttree/worktrees/{}-{}",
                result["task_id"].as_str().unwrap(),
                c["variant_name"].as_str().unwrap()
            )
        );
    }
    let cids: Vec<String> = candidates
        .iter()
        .map(|c| c["id"].as_str().expect("id").to_string())
        .collect();

    // Both candidates reach READY through the notification stream.
    for cid in &cids {
        client.wait_state(cid, "READY", Duration::from_secs(20));
    }

    // Persistence: READY rows with started/finished stamps, zero exit, and a
    // task that the arbitrator closes (winner picked, status COMPLETED).
    let store = open_store(&repo.root);
    for cid in &cids {
        let candidate = store.get_candidate(cid).expect("db").expect("row");
        assert_eq!(
            candidate.status.as_ref().ok(),
            Some(&CandidateStatus::Ready)
        );
        assert!(candidate.started_at.is_some());
        assert!(candidate.finished_at.is_some());
        assert_eq!(candidate.exit_code, Some(0));
        assert!(candidate.head_commit.is_none(), "no commits expected");
    }
    let task_id = result["task_id"].as_str().expect("task id").to_string();

    // The arbitrator scores both identical candidates (S_tests = 100, no
    // diff -> 0.5 * 100) and promotes the variant-name-lowest as winner.
    client.wait_task_status(&task_id, "COMPLETED", Duration::from_secs(10));
    let task = store.get_task(&task_id).expect("db").expect("task");
    assert_eq!(task.status.as_ref().ok(), Some(&TaskStatus::Completed));
    let winner_id = task.winner_candidate_id.expect("winner selected");
    assert!(cids.contains(&winner_id), "winner {winner_id} not a member");
    // Both variants are logically identical; the winner is whichever scored
    // higher (they differ only by sub-millisecond execution time). The score
    // is S_tests=100 scaled by weights minus a negligible time penalty.
    let winner = store.get_candidate(&winner_id).unwrap().unwrap();
    assert!(
        winner.score > 49.99 && winner.score <= 50.0,
        "{}",
        winner.score
    );
    let loser = store
        .get_candidate(cids.iter().find(|c| **c != winner_id).unwrap())
        .unwrap()
        .unwrap();
    assert!((loser.score - winner.score).abs() < 0.1, "{loser:?}");

    // GetStatus reflects the same data on the wire.
    let response = client.request(2, "GetStatus", json!({"task_id": task_id}));
    let task_view = &response["result"]["task"];
    assert_eq!(task_view["status"], "COMPLETED");
    assert_eq!(
        task_view["winner_candidate_id"],
        json!(winner_id),
        "winner surfaced on the wire"
    );
    assert_eq!(task_view["candidates"].as_array().map(Vec::len), Some(2));

    // Worktrees exist on disk with a branch per variant.
    let branches = git(
        &repo.root,
        &["branch", "--list", &format!("ghosttree/{task_id}/*")],
    );
    assert!(branches.contains("ghosttree/"), "{branches}");

    // Raw capture logs exist per candidate (SPEC §6.1 naming).
    for (idx, cid) in cids.iter().enumerate() {
        let log = repo
            .root
            .join(".ghosttree")
            .join("logs")
            .join(&task_id)
            .join(format!("{cid}.raw.log"));
        let content = std::fs::read_to_string(&log).expect("raw log");
        assert!(content.contains("working"), "{idx}");
    }

    daemon.stop();
}

#[test]
fn test_cmd_grades_ready_and_failed_and_logs_runs() {
    let repo = TestRepo::new();
    let daemon = TestDaemon::start(&repo.root);
    let mut client = Client::connect(&daemon.socket);

    // Passing harness -> READY.
    let pass = client.request(
        1,
        "SpawnSwarm",
        json!({
            "prompt": "graded pass",
            "variants": [{"name": "ok", "agent_cmd": "echo building; exit 0"}],
            "test_cmd": "echo RUNNING-TESTS-PASS; exit 0"
        }),
    );
    let cid = pass["result"]["candidates"][0]["id"]
        .as_str()
        .expect("id")
        .to_string();
    client.wait_state(&cid, "READY", Duration::from_secs(30));

    let store = open_store(&repo.root);
    let events = store.list_events(&cid).expect("events");
    assert!(
        events
            .iter()
            .any(|e| matches!(e.event_type.as_ref(), Ok(EventType::TestRun))),
        "expected a TEST_RUN event, got {events:?}"
    );

    // Failing harness -> FAILED, with the harness output captured.
    let fail = client.request(
        2,
        "SpawnSwarm",
        json!({
            "prompt": "graded fail",
            "variants": [{"name": "bad", "agent_cmd": "echo building bad; exit 0"}],
            "test_cmd": "echo RUNNING-TESTS-FAIL; exit 3"
        }),
    );
    let task_id = fail["result"]["task_id"]
        .as_str()
        .expect("task id")
        .to_string();
    let cid2 = fail["result"]["candidates"][0]["id"]
        .as_str()
        .expect("id")
        .to_string();
    client.wait_state(&cid2, "FAILED", Duration::from_secs(30));

    let store = open_store(&repo.root);
    let candidate = store.get_candidate(&cid2).expect("db").expect("row");
    assert_eq!(
        candidate.status.as_ref().ok(),
        Some(&CandidateStatus::Failed)
    );
    // The agent exited 0; the harness decided the failure (Phase 3 keeps the
    // agent exit code and does not fake a test exit code).
    assert_eq!(candidate.exit_code, Some(0));

    let test_log = repo
        .root
        .join(".ghosttree")
        .join("logs")
        .join(&task_id)
        .join(format!("{cid2}.tests.log"));
    let content = std::fs::read_to_string(&test_log).expect("tests log");
    assert!(content.contains("RUNNING-TESTS-FAIL"), "{content}");

    daemon.stop();
}

#[test]
fn kill_candidate_terminates_the_agent_process_group() {
    let repo = TestRepo::new();
    let daemon = TestDaemon::start(&repo.root);
    let mut client = Client::connect(&daemon.socket);

    let spawn = client.request(
        1,
        "SpawnSwarm",
        json!({
            "prompt": "looper",
            "variants": [{"name": "sleepy", "agent_cmd": "sleep 120"}],
            "timeout_secs": 60
        }),
    );
    let cid = spawn["result"]["candidates"][0]["id"]
        .as_str()
        .expect("id")
        .to_string();
    client.wait_state(&cid, "WORKING", Duration::from_secs(5));

    let response = client.request(2, "KillCandidate", json!({"candidate_id": cid}));
    assert_eq!(response["result"]["killed"], true);
    assert_eq!(response["result"]["was_live"], true);

    // Termination is reported through the state stream and persisted.
    client.wait_state(&cid, "FAILED", Duration::from_secs(10));

    let store = open_store(&repo.root);
    let candidate = store.get_candidate(&cid).expect("db").expect("row");
    assert_eq!(
        candidate.status.as_ref().ok(),
        Some(&CandidateStatus::Failed)
    );
    assert!(candidate.finished_at.is_some());
    assert_eq!(candidate.exit_code, None, "killed by signal, not exit");
    assert!(candidate.head_commit.is_none());

    // Killing an already-terminal candidate is a params error.
    let response = client.request(3, "KillCandidate", json!({"candidate_id": cid}));
    assert_eq!(response["error"]["code"], -32602);

    daemon.stop();
}

#[test]
fn idle_timeout_fails_a_stalled_agent() {
    let repo = TestRepo::new();
    let daemon = TestDaemon::start(&repo.root);
    let mut client = Client::connect(&daemon.socket);

    let started = Instant::now();
    let spawn = client.request(
        1,
        "SpawnSwarm",
        json!({
            "prompt": "watchdog",
            "variants": [{"name": "stalled", "agent_cmd": "sleep 90"}],
            "timeout_secs": 1
        }),
    );
    let cid = spawn["result"]["candidates"][0]["id"]
        .as_str()
        .expect("id")
        .to_string();
    client.wait_state(&cid, "WORKING", Duration::from_secs(5));
    client.wait_state(&cid, "FAILED", Duration::from_secs(10));
    assert!(started.elapsed() < Duration::from_secs(10));

    let store = open_store(&repo.root);
    let candidate = store.get_candidate(&cid).expect("db").expect("row");
    assert_eq!(
        candidate.status.as_ref().ok(),
        Some(&CandidateStatus::Failed)
    );
    let events = store.list_events(&cid).expect("events");
    let syscall_payloads: Vec<&str> = events
        .iter()
        .filter(|e| matches!(e.event_type.as_ref(), Ok(EventType::Syscall)))
        .map(|e| e.payload.as_str())
        .collect();
    assert!(
        syscall_payloads.iter().any(|p| p.contains("SPEC §8")),
        "expected the idle-timeout Syscall event, got {syscall_payloads:?}"
    );

    daemon.stop();
}

#[test]
fn attach_output_replays_and_follows_a_running_agent() {
    let repo = TestRepo::new();
    let daemon = TestDaemon::start(&repo.root);
    let mut client = Client::connect(&daemon.socket);

    let spawn = client.request(
        1,
        "SpawnSwarm",
        json!({
            "prompt": "chatty agent",
            "variants": [{
                "name": "chatter",
                "agent_cmd": "for i in 1 2 3; do echo tick-$i; sleep 0.4; done; exit 7"
            }]
        }),
    );
    let cid = spawn["result"]["candidates"][0]["id"]
        .as_str()
        .expect("id")
        .to_string();
    client.wait_state(&cid, "WORKING", Duration::from_secs(5));

    // Follow while running: attach ack, chunk notifications, then end.
    client.send_raw_line(
        &json!({"jsonrpc": "2.0", "id": 9, "method": "AttachOutput",
                "params": {"candidate_id": cid, "follow": true}})
        .to_string(),
    );
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut acked = false;
    let mut chunks = String::new();
    loop {
        assert!(Instant::now() < deadline, "attach stream did not finish");
        let frame = client.read_frame();
        if is_response_for(&frame, 9) {
            acked = true;
            assert_eq!(frame["result"]["attached"], true);
            assert_eq!(frame["result"]["follow"], true);
            continue;
        }
        if frame.get("method").and_then(Value::as_str) == Some("AttachOutput") {
            let params = &frame["params"];
            if params.get("candidate_id").and_then(Value::as_str) == Some(cid.as_str()) {
                match params["kind"].as_str() {
                    Some("chunk") => chunks.push_str(params["chunk"].as_str().expect("chunk text")),
                    Some("end") => {
                        assert!(
                            params["done"].as_bool().expect("done flag"),
                            "attach stream ended uncleanly"
                        );
                        break;
                    }
                    other => panic!("unexpected AttachOutput frame kind {other:?}"),
                }
            } else {
                client.seen.push(frame); // unrelated candidate's frames
            }
        } else if frame.get("method").is_some() {
            client.seen.push(frame); // state changes observed while attached
        }
    }
    assert!(acked, "AttachOutput ack missing before notifications");
    for tick in ["tick-1", "tick-2", "tick-3"] {
        assert!(chunks.contains(tick), "chunks missing {tick}: {chunks}");
    }
    // The agent exits 7, so the run FAILs even though the attach finished.
    client.wait_state(&cid, "FAILED", Duration::from_secs(10));

    // Replay of the finished candidate: whole log, then a clean end.
    client.send_raw_line(
        &json!({"jsonrpc": "2.0", "id": 10, "method": "AttachOutput",
                "params": {"candidate_id": cid, "follow": false}})
        .to_string(),
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut replay = String::new();
    let mut acked2 = false;
    loop {
        assert!(Instant::now() < deadline, "replay did not finish");
        let frame = client.read_frame();
        if is_response_for(&frame, 10) {
            acked2 = true;
            continue;
        }
        if frame.get("method").and_then(Value::as_str) == Some("AttachOutput") {
            let params = &frame["params"];
            assert_eq!(params["candidate_id"], json!(cid));
            match params["kind"].as_str() {
                Some("chunk") => replay.push_str(params["chunk"].as_str().expect("chunk")),
                Some("end") => {
                    assert!(
                        params["done"].as_bool().expect("done flag"),
                        "replay ended uncleanly"
                    );
                    break;
                }
                other => panic!("unexpected replay frame {other:?}"),
            }
        } else if frame.get("method").is_some() {
            client.seen.push(frame);
        }
    }
    assert!(acked2);
    assert!(replay.contains("tick-3"), "{replay}");

    daemon.stop();
}

#[test]
fn concurrent_connections_share_the_daemon_state() {
    let repo = TestRepo::new();
    let daemon = TestDaemon::start(&repo.root);

    // One swarm carrying state that all clients must observe.
    let mut main = Client::connect(&daemon.socket);
    let spawn = main.request(
        1,
        "SpawnSwarm",
        json!({
            "prompt": "concurrency probe",
            "variants": [{"name": "solo", "agent_cmd": "exit 0"}]
        }),
    );
    let task_id = spawn["result"]["task_id"]
        .as_str()
        .expect("task id")
        .to_string();
    let cid = spawn["result"]["candidates"][0]["id"]
        .as_str()
        .expect("id")
        .to_string();
    main.wait_state(&cid, "READY", Duration::from_secs(20));
    // The runner + arbitrator settle asynchronously; wait for the endpoint
    // before the concurrent readers start asserting on it.
    main.wait_task_status(&task_id, "COMPLETED", Duration::from_secs(20));

    // Four clients hammering GetStatus concurrently.
    let mut joins = Vec::new();
    for worker in 0..4 {
        let socket = daemon.socket.clone();
        let tid = task_id.clone();
        joins.push(thread::spawn(move || -> anyhow::Result<()> {
            let mut client = Client::connect(&socket);
            for i in 0..15 {
                let id = (worker * 100 + i) as i64;
                let response = client.request(id, "GetStatus", json!({"task_id": tid}));
                assert_eq!(
                    response["result"]["task"]["status"], "COMPLETED",
                    "unexpected snapshot from client {worker}"
                );
            }
            Ok(())
        }));
    }
    for join in joins {
        join.join().expect("rpc worker").expect("rpc worker ok");
    }

    // Plus a second fresh spawn from another client to interleave writes.
    let mut second = Client::connect(&daemon.socket);
    let spawn2 = second.request(
        2,
        "SpawnSwarm",
        json!({
            "prompt": "second swarm",
            "variants": [{"name": "also-solo", "agent_cmd": "exit 0"}]
        }),
    );
    let cid2 = spawn2["result"]["candidates"][0]["id"]
        .as_str()
        .expect("id")
        .to_string();
    second.wait_state(&cid2, "READY", Duration::from_secs(20));

    let full = main.request(3, "GetStatus", json!({}));
    assert_eq!(full["result"]["tasks"].as_array().map(Vec::len), Some(2));

    daemon.stop();
}

#[test]
fn boot_reconcile_recovers_orphans_per_spec8() {
    // Seed state that looks like a daemon died mid-run: WORKING candidate
    // whose process no longer exists, plus a never-started INITIALIZING row.
    let repo = TestRepo::new();
    {
        std::fs::create_dir_all(repo.root.join(".ghosttree")).expect("ghost dir");
        let store = Store::open(repo.root.join(".ghosttree").join("ghosttree.db"))
            .expect("seed store open");
        store
            .insert_task(&NewTask {
                id: "task-orphan".into(),
                title: "orphaned run".into(),
                prompt: "recovered at boot".into(),
                base_commit: git(&repo.root, &["rev-parse", "HEAD"]),
                base_branch: "main".into(),
                strategy: ghosttree::db::TaskStrategy::Competing,
            })
            .expect("seed task");
        let dead_pid = spawn_and_reap_dead_pid();
        store
            .insert_candidate(&NewCandidate {
                id: "cand-orphan".into(),
                task_id: "task-orphan".into(),
                variant_name: "ghost".into(),
                worktree_path: ".ghosttree/worktrees/task-orphan-ghost".into(),
                branch_name: "ghosttree/task-orphan/ghost".into(),
                agent_cmd: "sleep 999".into(),
            })
            .expect("seed candidate");
        store
            .set_candidate_status("cand-orphan", CandidateStatus::Working)
            .expect("seed status");
        store
            .set_candidate_pid("cand-orphan", Some(dead_pid as i64))
            .expect("seed pid");

        store
            .insert_candidate(&NewCandidate {
                id: "cand-unstarted".into(),
                task_id: "task-orphan".into(),
                variant_name: "ghost2".into(),
                worktree_path: ".ghosttree/worktrees/task-orphan-ghost2".into(),
                branch_name: "ghosttree/task-orphan/ghost2".into(),
                agent_cmd: "sleep 999".into(),
            })
            .expect("seed candidate 2");
        store
            .set_candidate_status("cand-unstarted", CandidateStatus::Initializing)
            .expect("seed status 2");
    }

    // The daemon boots, prunes, terminates unowned groups, and marks rows.
    let daemon = TestDaemon::start(&repo.root);
    let mut client = Client::connect(&daemon.socket);

    // Allow the boot reconcile a beat (it runs before bind, but keep the
    // assertion robust on slow disks).
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut orphan_view = None;
    while Instant::now() < deadline {
        let response = client.request(1, "GetStatus", json!({"task_id": "task-orphan"}));
        let task = &response["result"]["task"];
        if task["status"] == "ABORTED" {
            orphan_view = Some(task.clone());
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    let task = orphan_view.expect("task did not reconcile to ABORTED");
    assert_eq!(task["status"], "ABORTED");
    let statuses: Vec<(String, String)> = task["candidates"]
        .as_array()
        .expect("candidates")
        .iter()
        .map(|c| {
            (
                c["id"].as_str().expect("id").to_string(),
                c["status"].as_str().expect("status").to_string(),
            )
        })
        .collect();
    assert_eq!(statuses.len(), 2);
    for (_id, status) in statuses {
        assert_eq!(status, "FAILED");
    }

    // Fresh spawns keep working after reconciliation.
    let spawn = client.request(
        2,
        "SpawnSwarm",
        json!({
            "prompt": "post-recovery spawn",
            "variants": [{"name": "fresh", "agent_cmd": "exit 0"}],
            "timeout_secs": 30
        }),
    );
    let cid = spawn["result"]["candidates"][0]["id"]
        .as_str()
        .expect("id")
        .to_string();
    client.wait_state(&cid, "READY", Duration::from_secs(20));

    daemon.stop();
}

/// A pid that is guaranteed dead for the duration of a reconcile: spawn a
/// short-lived helper, wait for it, and hand back its (reaped) pid.
fn spawn_and_reap_dead_pid() -> u32 {
    let mut child = StdCommand::new("true")
        .spawn()
        .expect("spawn helper for dead pid");
    let pid = child.id();
    assert_ne!(pid, std::process::id(), "refusing to reuse our own pid");
    // Reap: an unreaped child lingers as a zombie (kill(pid,0) == alive).
    let _status = child.wait().expect("reap helper");
    // Wait until the kernel considers it gone (kill(pid,0) -> ESRCH).
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if !ghosttree::daemon::pid_alive(pid as i32) {
            break;
        }
        assert!(Instant::now() < deadline, "helper never died");
        thread::sleep(Duration::from_millis(10));
    }
    pid
}

// ---------------------------------------------------------------------------
// Real-binary lifecycle (`gt daemon start/stop`, foreground SIGTERM).
// ---------------------------------------------------------------------------

fn gt() -> StdCommand {
    StdCommand::new(env!("CARGO_BIN_EXE_gt"))
}

fn wait_for_file(path: &Path, timeout: Duration, description: &str) {
    let deadline = Instant::now() + timeout;
    while !path.exists() {
        assert!(Instant::now() < deadline, "{description} did not appear");
        thread::sleep(Duration::from_millis(25));
    }
}

fn wait_gone(path: &Path, timeout: Duration, description: &str) {
    let deadline = Instant::now() + timeout;
    while path.exists() {
        assert!(Instant::now() < deadline, "{description} did not disappear");
        thread::sleep(Duration::from_millis(25));
    }
}

#[test]
fn binary_daemon_start_stop_logs_and_refuses_double_start() {
    let repo = TestRepo::new();
    gt().arg("init")
        .current_dir(&repo.root)
        .status()
        .expect("gt init")
        .success()
        .then_some(())
        .expect("init failed");

    let socket = repo.root.join(".ghosttree/ghosttree.sock");
    let pid_file = repo.root.join(".ghosttree/daemon.pid");

    // Background start comes up ready to answer RPCs.
    let out = gt()
        .args(["daemon", "start"])
        .current_dir(&repo.root)
        .output()
        .expect("spawn gt daemon start");
    assert!(
        out.status.success(),
        "gt daemon start failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    wait_for_file(&socket, Duration::from_secs(10), "daemon socket");
    wait_for_file(&pid_file, Duration::from_secs(10), "pid file");

    let mut client = Client::connect(&socket);
    let response = client.request(1, "GetStatus", json!({}));
    assert_eq!(response["result"]["tasks"], json!([]));

    // A second daemon on the same repo is refused (pid-file liveness).
    let out = gt()
        .args(["daemon", "start"])
        .current_dir(&repo.root)
        .output()
        .expect("spawn second gt daemon start");
    assert!(
        !out.status.success(),
        "second daemon start must fail while one is running"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("already running"), "{stderr}");

    // The original daemon is unharmed and stops cleanly.
    let mut client = Client::connect(&socket);
    let response = client.request(2, "GetStatus", json!({}));
    assert_eq!(response["result"]["tasks"], json!([]));

    let out = gt()
        .args(["daemon", "stop"])
        .current_dir(&repo.root)
        .output()
        .expect("spawn gt daemon stop");
    assert!(
        out.status.success(),
        "gt daemon stop failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    wait_gone(&socket, Duration::from_secs(10), "socket");
    wait_gone(&pid_file, Duration::from_secs(12), "pid file");

    // Stopping with nothing around is an error, not a success.
    let out = gt()
        .args(["daemon", "stop"])
        .current_dir(&repo.root)
        .output()
        .expect("spawn second gt daemon stop");
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("no daemon running"), "{stderr}");

    // The daemon log captured the lifecycle (SPEC §2.1 daemon.log).
    let daemon_log = repo.root.join(".ghosttree/logs/daemon.log");
    let content = std::fs::read_to_string(&daemon_log).expect("daemon log");
    assert!(content.contains("listening"), "{content}");
    assert!(content.contains("shutdown complete"), "{content}");
}

#[test]
fn foreground_daemon_shuts_down_on_sigterm() {
    let repo = TestRepo::new();
    gt().arg("init")
        .current_dir(&repo.root)
        .status()
        .expect("gt init")
        .success()
        .then_some(())
        .expect("init failed");

    let socket = repo.root.join(".ghosttree/ghosttree.sock");
    let pid_file = repo.root.join(".ghosttree/daemon.pid");

    let mut child = gt()
        .args(["daemon", "start", "--foreground"])
        .current_dir(&repo.root)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn foreground daemon");
    wait_for_file(&socket, Duration::from_secs(10), "daemon socket");

    // SIGTERM -> clean shutdown: files removed, exit status success.
    // SAFETY: plain process-directed signal to our own child.
    let rc = unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
    assert_eq!(rc, 0, "failed to SIGTERM the foreground daemon");

    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        match child.try_wait().expect("poll foreground daemon") {
            Some(status) => break status,
            None => {
                assert!(Instant::now() < deadline, "daemon ignored SIGTERM");
                thread::sleep(Duration::from_millis(50));
            }
        }
    };
    assert!(status.success(), "SIGTERM shutdown exited {status}");
    wait_gone(&socket, Duration::from_secs(5), "socket");
    wait_gone(&pid_file, Duration::from_secs(5), "pid file");
}
