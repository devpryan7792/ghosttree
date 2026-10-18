//! JSON-RPC 2.0 over newline-delimited frames (SPEC §6.2).
//!
//! Wire contract:
//! * Requests and notifications are single-line JSON objects (`\n` terminator).
//! * `SpawnSwarm` / `GetStatus` / `KillCandidate` are requests (id + response).
//! * `CandidateStateChanged` is a daemon→client notification; a client
//!   request with that method name is rejected with `-32601`.
//! * `AttachOutput` answers once with `{"attached":true,...}` and then
//!   streams `AttachOutput` notifications (`kind:"chunk"`, final
//!   `kind:"end"`) — the multiplexing pattern permitted for one id under
//!   JSON-RPC 2.0.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::{json, Value};

use crate::daemon::DaemonShared;
use crate::db::{views, CandidateStatus};

/// Guard-rail on request size (prompt text can be long, not pathological).
const MAX_LINE_BYTES: usize = 1 << 20;

/// How often the attach tail polls for new log bytes / candidate finish.
const ATTACH_POLL: Duration = Duration::from_millis(40);

/// JSON-RPC error (-32700..-32603 standard; -32000 range is server-defined).
#[derive(Debug, Clone)]
pub(crate) struct RpcError {
    pub code: i32,
    pub message: String,
}

impl RpcError {
    pub fn invalid_params(message: impl Into<String>) -> Self {
        Self {
            code: -32602,
            message: message.into(),
        }
    }
    pub fn method_not_found(message: impl Into<String>) -> Self {
        Self {
            code: -32601,
            message: message.into(),
        }
    }
    pub fn internal(message: impl Into<String>) -> Self {
        Self {
            code: -32603,
            message: message.into(),
        }
    }
}

pub(crate) type RpcResult<T> = Result<T, RpcError>;

/// Transport-agnostic socket requirement: duplex, cloneable for the writer
/// half, and owned by the connection thread.
pub trait RpcSocket: Read + Write + Send + 'static {
    /// Duplicate the socket so a writer thread can block on it independently.
    fn try_clone_socket(&self) -> std::io::Result<Self>
    where
        Self: Sized;
}

impl RpcSocket for UnixStream {
    fn try_clone_socket(&self) -> std::io::Result<Self>
    where
        Self: Sized,
    {
        UnixStream::try_clone(self)
    }
}

impl RpcSocket for TcpStream {
    fn try_clone_socket(&self) -> std::io::Result<Self>
    where
        Self: Sized,
    {
        TcpStream::try_clone(self)
    }
}

/// Handle one client connection until EOF/error.
pub(crate) fn handle_socket<S: RpcSocket>(stream: S, shared: Arc<DaemonShared>) {
    let Ok(writer) = stream.try_clone_socket() else {
        // Socket died before it could be served; nothing to clean up yet.
        return;
    };
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    let writer_thread = thread::Builder::new()
        .name("gt-ipc-writer".into())
        .spawn(move || {
            let mut sink = writer;
            for frame in rx {
                if sink.write_all(frame.as_bytes()).is_err() {
                    break;
                }
                let _ = sink.flush();
            }
        });
    let Ok(writer_thread) = writer_thread else {
        return;
    };

    let client_id = shared.next_client_id.fetch_add(1, Ordering::SeqCst);
    shared
        .clients
        .lock()
        .expect("clients poisoned")
        .insert(client_id, tx.clone());

    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => break, // peer closed
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
        let trimmed = line.trim_end_matches(['\n', '\r']);
        if trimmed.is_empty() {
            continue;
        }
        if line.len() > MAX_LINE_BYTES {
            send_error(
                &tx,
                None,
                &RpcError::internal(format!("request exceeds {MAX_LINE_BYTES} bytes")).code(-32700),
            );
            continue;
        }
        match serde_json::from_str::<Value>(trimmed) {
            Ok(message) => dispatch(&shared, &tx, message),
            Err(e) => {
                send_error(
                    &tx,
                    None,
                    &RpcError::internal(format!("parse error: {e}")).code(-32700),
                );
                continue;
            }
        }
    }

    shared
        .clients
        .lock()
        .expect("clients poisoned")
        .remove(&client_id);
    drop(tx); // writer drains pending frames, then closes the socket
              // Give the writer a bounded window to flush before the handle drops; a
              // wedged peer must not be able to hold the daemon lifecycle hostage.
    let deadline = Instant::now() + Duration::from_millis(500);
    while !writer_thread.is_finished() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
}

impl RpcError {
    fn code(mut self, code: i32) -> Self {
        self.code = code;
        self
    }
}

fn send_result(tx: &Sender<String>, id: Value, result: Value) {
    let mut line = json!({"jsonrpc": "2.0", "result": result, "id": id}).to_string();
    line.push('\n');
    let _ = tx.send(line);
}

fn send_error(tx: &Sender<String>, id: Option<Value>, err: &RpcError) {
    let mut line = json!({
        "jsonrpc": "2.0",
        "id": id.unwrap_or(Value::Null),
        "error": {"code": err.code, "message": err.message},
    })
    .to_string();
    line.push('\n');
    let _ = tx.send(line);
}

fn respond(tx: &Sender<String>, id: Value, result: RpcResult<Value>) {
    match result {
        Ok(value) => send_result(tx, id, value),
        Err(e) => send_error(tx, Some(id), &e),
    }
}

/// Route one incoming JSON-RPC message.
pub(crate) fn dispatch(shared: &Arc<DaemonShared>, tx: &Sender<String>, message: Value) {
    let Value::Object(fields) = &message else {
        send_error(
            tx,
            None,
            &RpcError::internal("request must be a JSON object").code(-32600),
        );
        return;
    };
    if let Some(version) = fields.get("jsonrpc").and_then(Value::as_str) {
        if version != "2.0" {
            send_error(
                tx,
                fields.get("id").cloned(),
                &RpcError::internal(format!(
                    "unsupported jsonrpc version {version:?}, want \"2.0\""
                ))
                .code(-32600),
            );
            return;
        }
    }

    // An absent or null id marks a notification: the daemon defines no
    // client-side notifications, so these are accepted but never answered.
    let id = fields
        .get("id")
        .cloned()
        .filter(|id| id.is_string() || id.is_number());
    let Some(id) = id else { return };

    let method = fields.get("method").and_then(Value::as_str).unwrap_or("");
    let params = fields.get("params").cloned().unwrap_or_else(|| json!({}));

    match method {
        "GetStatus" => respond(tx, id, get_status(shared, &params)),
        "SpawnSwarm" => respond(tx, id, spawn_swarm(shared, &params)),
        "KillCandidate" => respond(tx, id, kill_candidate(shared, &params)),
        "AttachOutput" => start_attachment(shared, tx, id, &params),
        "CandidateStateChanged" => {
            let err = RpcError::method_not_found(
                "CandidateStateChanged is a daemon notification (broadcast to subscribers), not a callable method",
            );
            send_error(tx, Some(id), &err);
        }
        other => {
            let err = RpcError::method_not_found(format!(
                "unknown method {other:?}; supported: GetStatus, SpawnSwarm, KillCandidate, AttachOutput"
            ));
            send_error(tx, Some(id), &err);
        }
    }
}

// ---------------------------------------------------------------------------
// Method parameter/result types.
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub(crate) struct SpawnSwarmParams {
    /// optional caller-chosen task id (must not exist yet)
    #[serde(default)]
    pub task_id: Option<String>,
    pub prompt: String,
    /// agent variants to run (at least one)
    #[serde(default)]
    pub variants: Vec<VariantSpec>,
    /// optional test command executed after each agent exits (Phase 3 runs a
    /// basic pass/fail harness; full scoring lands with the Phase 4 arbitrator)
    #[serde(default)]
    pub test_cmd: Option<String>,
    /// per-candidate idle timeout seconds (SPEC §8, default 900)
    #[serde(default)]
    pub timeout_secs: Option<u64>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct VariantSpec {
    #[serde(default)]
    pub name: Option<String>,
    pub agent_cmd: String,
}

#[derive(Debug, Deserialize)]
struct CandidateRef {
    candidate_id: String,
}

#[derive(Debug, Deserialize)]
struct AttachParams {
    candidate_id: String,
    /// tail while the candidate runs (default: replay once then stop)
    #[serde(default)]
    follow: bool,
}

#[derive(Debug, Deserialize)]
struct StatusParams {
    #[serde(default)]
    task_id: Option<String>,
    #[serde(default)]
    candidate_id: Option<String>,
    /// include COMPLETED/ABORTED tasks (default true for daemon clients)
    #[serde(default = "default_true")]
    all: bool,
}

fn default_true() -> bool {
    true
}

#[derive(serde::Serialize)]
struct AttachResult {
    attached: bool,
    candidate_id: String,
    path: String,
    follow: bool,
    encoding: &'static str,
}

fn parse_params<T: for<'de> Deserialize<'de>>(params: &Value) -> RpcResult<T> {
    serde_json::from_value(params.clone())
        .map_err(|e| RpcError::invalid_params(format!("invalid params: {e}")))
}

fn db_err(e: impl std::fmt::Display) -> RpcError {
    RpcError::internal(format!("state store error: {e}"))
}

// ---------------------------------------------------------------------------
// Method bodies.
// ---------------------------------------------------------------------------

fn get_status(shared: &Arc<DaemonShared>, params: &Value) -> RpcResult<Value> {
    let p: StatusParams = parse_params(params)?;
    {
        let store = shared.store.lock().expect("store poisoned");
        if let Some(cid) = p.candidate_id.as_deref() {
            let candidate = store
                .get_candidate(cid)
                .map_err(db_err)?
                .ok_or_else(|| RpcError::invalid_params(format!("unknown candidate {cid:?}")))?;
            let view = views::CandidateView::from_row(&candidate);
            return Ok(json!({"candidate": view}));
        }
        if let Some(tid) = p.task_id.as_deref() {
            let task = store
                .get_task(tid)
                .map_err(db_err)?
                .ok_or_else(|| RpcError::invalid_params(format!("unknown task {tid:?}")))?;
            let candidates = store
                .list_candidates(tid)
                .map_err(db_err)?
                .iter()
                .map(views::CandidateView::from_row)
                .collect();
            return Ok(json!({"task": views::TaskView::from_row(&task, candidates)}));
        }
        let snapshot =
            views::snapshot(&store, p.all).map_err(|e| RpcError::internal(db_err(e).message))?;
        serde_json::to_value(snapshot)
            .map_err(|e| RpcError::internal(format!("snapshot serialization failed: {e}")))
    }
}

fn spawn_swarm(shared: &Arc<DaemonShared>, params: &Value) -> RpcResult<Value> {
    let p: SpawnSwarmParams = parse_params(params)?;
    if p.variants.is_empty() {
        return Err(RpcError::invalid_params(
            "SpawnSwarm requires at least 1 variant",
        ));
    }
    let info = crate::daemon::state::spawn_swarm(shared, p)
        .map_err(|(code, message)| RpcError { code, message })?;
    serde_json::to_value(info)
        .map_err(|e| RpcError::internal(format!("swarm result serialization failed: {e}")))
}

fn kill_candidate(shared: &Arc<DaemonShared>, params: &Value) -> RpcResult<Value> {
    let p: CandidateRef = parse_params(params)?;
    let was_live = crate::daemon::state::kill_candidate(shared, &p.candidate_id)
        .map_err(|(code, message)| RpcError { code, message })?;
    Ok(json!({
        "killed": true,
        "candidate_id": p.candidate_id,
        "was_live": was_live,
    }))
}

/// `AttachOutput`: validate the request, answer once, then stream the raw
/// capture log via notifications from a dedicated tail thread.
fn start_attachment(shared: &Arc<DaemonShared>, tx: &Sender<String>, id: Value, params: &Value) {
    let p: AttachParams = match parse_params(params) {
        Ok(p) => p,
        Err(e) => return send_error(tx, Some(id), &e),
    };
    let log_path: PathBuf;
    let candidate_id = p.candidate_id.clone();
    {
        let store = shared.store.lock().expect("store poisoned");
        let candidate = match store.get_candidate(&candidate_id) {
            Ok(Some(c)) => c,
            Ok(None) => {
                return send_error(
                    tx,
                    Some(id),
                    &RpcError::invalid_params(format!("unknown candidate {candidate_id:?}")),
                );
            }
            Err(e) => return send_error(tx, Some(id), &db_err(e)),
        };
        log_path = shared.candidate_log_path(&candidate.task_id, &candidate_id);
    }
    if !p.follow && !log_path.exists() {
        return send_error(
            tx,
            Some(id),
            &RpcError::invalid_params(format!(
                "no capture log for candidate {candidate_id:?} yet (use follow:true to wait)"
            )),
        );
    }

    send_result(
        tx,
        id,
        json!(AttachResult {
            attached: true,
            path: log_path.display().to_string(),
            encoding: "text",
            follow: p.follow,
            candidate_id: candidate_id.clone(),
        }),
    );

    // The tail thread needs its own outbound handle; a disconnected client is
    // detected on the first failed send and ends the stream.
    let tail_shared = shared.clone();
    let tail_tx = tx.clone();
    thread::spawn(move || attach_loop(tail_shared, tail_tx, candidate_id, log_path, p.follow));
}

/// Tail (or replay) a candidate's raw capture log via notifications.
///
/// Contract: `follow == false` replays the bytes that exist at attach time
/// and ends at the first EOF; `follow == true` keeps polling until the
/// candidate stops running (or the client disconnects / the daemon exits).
fn attach_loop(
    shared: Arc<DaemonShared>,
    tx: Sender<String>,
    candidate_id: String,
    log_path: PathBuf,
    follow: bool,
) {
    // Wait for the capture file to appear while following; replay attaches
    // surface a missing path as an early `end` (the response already verified
    // existence, so this only fires on a racing removal).
    let mut file = loop {
        match std::fs::File::open(&log_path) {
            Ok(f) => break f,
            Err(_) if follow && !shared.shutdown.load(Ordering::SeqCst) => {
                thread::sleep(ATTACH_POLL);
            }
            Err(_) => {
                let _ = stream_end(&tx, &candidate_id, false);
                return;
            }
        }
    };

    let mut buf = [0u8; 8192];
    loop {
        match file.read(&mut buf) {
            Ok(0) => {
                // EOF: one pass is the replay contract; while following, keep
                // polling until the candidate leaves the active set.
                if !follow
                    || !candidate_running(&shared, &candidate_id)
                    || shared.shutdown.load(Ordering::SeqCst)
                {
                    break;
                }
                thread::sleep(ATTACH_POLL);
            }
            Ok(n) => {
                let text = String::from_utf8_lossy(&buf[..n]);
                if !stream_chunk(&tx, &candidate_id, &text) {
                    return; // client disconnected
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
    let _ = stream_end(&tx, &candidate_id, true);
}

fn stream_chunk(tx: &Sender<String>, candidate_id: &str, chunk: &str) -> bool {
    let frame = json!({
        "jsonrpc": "2.0",
        "method": "AttachOutput",
        "params": {
            "candidate_id": candidate_id,
            "kind": "chunk",
            "chunk": chunk,
        },
    });
    tx.send(frame.to_string() + "\n").is_ok()
}

fn stream_end(tx: &Sender<String>, candidate_id: &str, done: bool) -> bool {
    let frame = json!({
        "jsonrpc": "2.0",
        "method": "AttachOutput",
        "params": {
            "candidate_id": candidate_id,
            "kind": "end",
            "done": done,
        },
    });
    tx.send(frame.to_string() + "\n").is_ok()
}

fn candidate_running(shared: &DaemonShared, candidate_id: &str) -> bool {
    let store = shared.store.lock().expect("store poisoned");
    store
        .get_candidate(candidate_id)
        .ok()
        .flatten()
        .and_then(|c| c.status.ok())
        .map(|status| {
            matches!(
                status,
                CandidateStatus::Initializing | CandidateStatus::Working | CandidateStatus::Testing
            )
        })
        .unwrap_or(false)
}
