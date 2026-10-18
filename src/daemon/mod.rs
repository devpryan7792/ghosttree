//! GhostTree daemon (SPEC §5.1 `gt daemon`, §6 IPC, §8 recovery).
//!
//! The daemon owns the SQLite store, arbitrates candidate lifecycle state
//! transitions (SPEC §3.2), and exposes JSON-RPC 2.0 over a Unix domain
//! socket (`.ghosttree/ghosttree.sock`). An optional extra TCP listener is
//! supported for the spec'd `--port <n>` flag.
//!
//! Threading model (deliberately std-only for Phase 3):
//! * one accept loop per transport (non-blocking + poll, so shutdown never
//!   depends on signal interruption semantics),
//! * one reader thread per connection; all outbound traffic for a connection
//!   funnels through a single writer thread channel to preserve framing,
//! * one runner thread per candidate agent process,
//! * a Mutex'd [`Store`] shared across all of them.

pub mod client;
pub mod ipc;
pub mod state;

use std::collections::HashMap;
use std::fs;
use std::io;
use std::net::{TcpListener, TcpStream};
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::json;

use crate::db::Store;

/// Layout constants (SPEC §2.1 file system hierarchy).
pub const GHOST_DIR: &str = ".ghosttree";
pub const DB_FILE: &str = "ghosttree.db";
pub const SOCKET_FILE: &str = "ghosttree.sock";
pub const PID_FILE: &str = "daemon.pid";
pub const DAEMON_LOG_FILE: &str = "daemon.log";

/// Longest wait for in-flight connections to drain on shutdown.
pub const CONN_DRAIN: Duration = Duration::from_millis(1_500);
/// Longest wait for candidates to die after broadcasting the kill signal.
pub const CANDIDATE_DRAIN: Duration = Duration::from_millis(2_500);
/// Accept-loop poll interval (doubles as shutdown latency budget).
pub const ACCEPT_POLL: Duration = Duration::from_millis(50);

type KillTx = Sender<()>;

/// Anything the IPC layer and candidate runners need from the daemon.
pub(crate) struct DaemonShared {
    pub root: PathBuf,
    pub store: Mutex<Store>,
    /// client id -> outbound frame channel (per-connection writer).
    pub clients: Mutex<HashMap<u64, Sender<String>>>,
    pub next_client_id: AtomicU64,
    /// candidate id -> kill signal into its runner thread.
    pub live: Mutex<HashMap<String, KillTx>>,
    /// Currently open RPC connections (for bounded shutdown drain).
    pub active_conns: AtomicUsize,
    pub shutdown: Arc<AtomicBool>,
}

impl DaemonShared {
    /// Path to a candidate's raw pty capture: `.ghosttree/logs/<task>/<candidate>.raw.log`.
    pub fn candidate_log_path(&self, task_id: &str, candidate_id: &str) -> PathBuf {
        self.ghost_dir()
            .join("logs")
            .join(task_id)
            .join(format!("{candidate_id}.raw.log"))
    }

    pub fn ghost_dir(&self) -> PathBuf {
        self.root.join(GHOST_DIR)
    }
}

static SHUTDOWN: OnceLock<Arc<AtomicBool>> = OnceLock::new();

/// The per-process shutdown flag, also written by signal handlers.
pub fn shutdown_flag() -> Arc<AtomicBool> {
    SHUTDOWN
        .get_or_init(|| Arc::new(AtomicBool::new(false)))
        .clone()
}

extern "C" fn on_signal(_sig: libc::c_int) {
    if let Some(flag) = SHUTDOWN.get() {
        flag.store(true, Ordering::SeqCst);
    }
}

/// Install SIGTERM/SIGINT handlers that set the shutdown flag. No SA_RESTART,
/// so blocking syscalls in this process get interrupted where we rely on it.
pub fn install_shutdown_signals() -> io::Result<()> {
    // Flag must exist before a signal can be delivered.
    let _ = shutdown_flag();
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = on_signal as *const () as usize;
        sa.sa_flags = 0;
        if libc::sigemptyset(&mut sa.sa_mask) != 0 {
            return Err(io::Error::last_os_error());
        }
        for sig in [libc::SIGTERM, libc::SIGINT] {
            if libc::sigaction(sig, &sa, std::ptr::null_mut()) != 0 {
                return Err(io::Error::last_os_error());
            }
        }
    }
    Ok(())
}

/// Options a daemon instance runs with.
#[derive(Debug, Clone)]
pub struct DaemonOptions {
    /// Primary repository root (usually `Repo::discover(..).root()`).
    pub root: PathBuf,
    /// Optional extra TCP listener on 127.0.0.1 for `gt daemon start --port`.
    pub tcp_port: Option<u16>,
}

impl DaemonOptions {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            tcp_port: None,
        }
    }

    pub fn socket_path(&self) -> PathBuf {
        self.root.join(GHOST_DIR).join(SOCKET_FILE)
    }

    pub fn pid_path(&self) -> PathBuf {
        self.root.join(GHOST_DIR).join(PID_FILE)
    }

    pub fn daemon_log_path(&self) -> PathBuf {
        self.root.join(GHOST_DIR).join("logs").join(DAEMON_LOG_FILE)
    }
}

/// Run a foreground daemon until SIGTERM/SIGINT (or the passed flag) fires.
pub fn serve(opts: &DaemonOptions) -> io::Result<()> {
    install_shutdown_signals()?;
    serve_with_shutdown(opts, shutdown_flag())
}

/// Serve until `shutdown` flips true. Returns after cleanup; safe to call in
/// tests on a background thread.
pub fn serve_with_shutdown(opts: &DaemonOptions, shutdown: Arc<AtomicBool>) -> io::Result<()> {
    let ghost = opts.root.join(GHOST_DIR);
    fs::create_dir_all(ghost.join("logs"))?;
    fs::create_dir_all(ghost.join("worktrees"))?;

    let current_pid = std::process::id() as i32;
    if let Some(existing) = read_pid(&opts.pid_path())? {
        if existing != current_pid && pid_alive(existing) {
            return Err(io::Error::other(format!(
                "daemon already running (pid {existing})"
            )));
        }
    }

    // Only a daemon with no live pid recorded may clear stale artifacts.
    let stale = match read_pid(&opts.pid_path())? {
        Some(pid) => !pid_alive(pid),
        None => true,
    };
    let socket_path = opts.socket_path();
    if stale {
        let _ = fs::remove_file(&socket_path);
        let _ = fs::remove_file(opts.pid_path());
    }

    let db_path = ghost.join(DB_FILE);
    let store = Store::open(&db_path).map_err(|e| {
        io::Error::other(format!(
            "failed to open state db {}: {e}",
            db_path.display()
        ))
    })?;

    let shared = Arc::new(DaemonShared {
        root: opts.root.clone(),
        store: Mutex::new(store),
        clients: Mutex::new(HashMap::new()),
        next_client_id: AtomicU64::new(1),
        live: Mutex::new(HashMap::new()),
        active_conns: AtomicUsize::new(0),
        shutdown: shutdown.clone(),
    });

    // SPEC §8: reconcile filesystem against state before accepting work.
    let recovered = state::reconcile(&shared).unwrap_or_else(|e| {
        eprintln!("warning: boot reconciliation failed: {e}");
        0
    });

    let listener = UnixListener::bind(&socket_path)?;
    let tcp = match opts.tcp_port {
        Some(port) => Some(TcpListener::bind(("127.0.0.1", port))?),
        None => None,
    };
    // pid file only after the socket is bound (readiness marker for `start`).
    fs::write(opts.pid_path(), format!("{current_pid}\n"))?;

    eprintln!(
        "[GhostTree] daemon listening: {} (tcp {:?})",
        socket_path.display(),
        tcp.as_ref().map(|l| l.local_addr().ok().map(|a| a.port()))
    );
    if recovered > 0 {
        eprintln!(
            "[GhostTree] boot recovery: marked {recovered} stale candidate(s) as FAILED (SPEC §8)"
        );
    }

    let accept_threads = start_accept_loops(&shared, listener, tcp);

    // Block until shutdown is requested.
    while !shutdown.load(Ordering::SeqCst) {
        thread::sleep(ACCEPT_POLL);
    }
    eprintln!("[GhostTree] shutdown requested; draining");

    // 1. Stop accepting new connections/executions.
    for h in accept_threads {
        let _ = h.join();
    }

    // 2. Ask every live candidate process group to terminate (SIGTERM ladder
    //    inside the runner via supervisor::ProcessGroup::terminate).
    {
        let live = shared.live.lock().expect("live map poisoned");
        for (candidate_id, kill) in live.iter() {
            let _ = kill.send(());
            eprintln!("[GhostTree] signaled candidate {candidate_id} to stop");
        }
    }
    let deadline = Instant::now() + CANDIDATE_DRAIN;
    while Instant::now() < deadline && !shared.live.lock().expect("live map poisoned").is_empty() {
        thread::sleep(ACCEPT_POLL);
    }

    // 3. Drop client channels so per-connection writers close their streams.
    shared.clients.lock().expect("clients poisoned").clear();
    let drain_until = Instant::now() + CONN_DRAIN;
    while Instant::now() < drain_until && shared.active_conns.load(Ordering::SeqCst) > 0 {
        thread::sleep(ACCEPT_POLL);
    }

    // 4. Clean filesystem ownership marks.
    let _ = fs::remove_file(&socket_path);
    let _ = fs::remove_file(opts.pid_path());
    eprintln!("[GhostTree] shutdown complete");
    Ok(())
}

fn start_accept_loops(
    shared: &Arc<DaemonShared>,
    unix: UnixListener,
    tcp: Option<TcpListener>,
) -> Vec<thread::JoinHandle<()>> {
    let mut handles = Vec::new();
    handles.push(spawn_accept_loop(shared.clone(), Transport::Unix(unix)));
    if let Some(tcp) = tcp {
        handles.push(spawn_accept_loop(shared.clone(), Transport::Tcp(tcp)));
    }
    handles
}

enum Transport {
    Unix(UnixListener),
    Tcp(TcpListener),
}

fn spawn_accept_loop(shared: Arc<DaemonShared>, transport: Transport) -> thread::JoinHandle<()> {
    match transport {
        Transport::Unix(listener) => thread::spawn(move || accept_loop(listener, shared)),
        Transport::Tcp(listener) => thread::spawn(move || accept_loop(listener, shared)),
    }
}

/// Minimal listener abstraction so the accept loop is transport-agnostic.
pub(crate) trait Listener {
    type Stream: ipc::RpcSocket;
    fn accept(&mut self) -> io::Result<Self::Stream>;
    fn set_nonblocking(&mut self, nonblocking: bool) -> io::Result<()>;
}

impl Listener for UnixListener {
    type Stream = UnixStream;
    fn accept(&mut self) -> io::Result<UnixStream> {
        UnixListener::accept(self).map(|(stream, _)| stream)
    }
    fn set_nonblocking(&mut self, nonblocking: bool) -> io::Result<()> {
        UnixListener::set_nonblocking(self, nonblocking)
    }
}

impl Listener for TcpListener {
    type Stream = TcpStream;
    fn accept(&mut self) -> io::Result<TcpStream> {
        TcpListener::accept(self).map(|(stream, _)| stream)
    }
    fn set_nonblocking(&mut self, nonblocking: bool) -> io::Result<()> {
        TcpListener::set_nonblocking(self, nonblocking)
    }
}

/// Generic accept loop: non-blocking accepts with a poll interval, exiting
/// cleanly once the shutdown flag flips.
fn accept_loop<L: Listener>(mut listener: L, shared: Arc<DaemonShared>) {
    if let Err(e) = listener.set_nonblocking(true) {
        eprintln!("[GhostTree] listener nonblocking setup failed: {e}");
        return;
    }
    while !shared.shutdown.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok(stream) => {
                shared.active_conns.fetch_add(1, Ordering::SeqCst);
                let conn_shared = shared.clone();
                let counter = shared.clone();
                thread::spawn(move || {
                    ipc::handle_socket(stream, conn_shared);
                    counter.active_conns.fetch_sub(1, Ordering::SeqCst);
                });
            }
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(ACCEPT_POLL);
            }
            Err(ref e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => {
                // Back off on unexpected accept errors; the loop keeps trying
                // until shutdown so a transient EMFILE does not kill the daemon.
                thread::sleep(Duration::from_millis(100));
            }
        }
    }
}

/// Broadcast a notification to every connected RPC client.
pub(crate) fn broadcast(shared: &DaemonShared, method: &str, params: serde_json::Value) {
    let frame = json!({"jsonrpc": "2.0", "method": method, "params": params});
    let mut line = frame.to_string();
    line.push('\n');
    let mut dead = Vec::new();
    {
        let clients = shared.clients.lock().expect("clients poisoned");
        for (id, tx) in clients.iter() {
            if tx.send(line.clone()).is_err() {
                dead.push(*id);
            }
        }
    }
    if !dead.is_empty() {
        let mut clients = shared.clients.lock().expect("clients poisoned");
        for id in dead {
            clients.remove(&id);
        }
    }
}

/// Read the recorded daemon pid, if any.
pub fn read_pid(pid_path: &Path) -> io::Result<Option<i32>> {
    match fs::read_to_string(pid_path) {
        Ok(text) => {
            let pid = text.trim();
            if pid.is_empty() {
                return Ok(None);
            }
            pid.parse::<i32>()
                .map(Some)
                .map_err(|_| io::Error::other(format!("corrupt pid file content {pid:?}")))
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// Is `pid` alive for the current user (EPERM also counts as alive)?
pub fn pid_alive(pid: i32) -> bool {
    match signal_pid(pid, 0) {
        Ok(()) => true,
        Err(e) => e.raw_os_error() != Some(libc::ESRCH),
    }
}

/// Send a raw signal to a pid (used by `gt daemon stop`).
pub fn signal_pid(pid: i32, sig: i32) -> io::Result<()> {
    // SAFETY: pure syscall wrapper.
    let rc = unsafe { libc::kill(pid, sig) };
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Launch a fresh foreground daemon detached from the current session
/// (`gt daemon start` without `--foreground`), then wait for readiness.
pub fn start_background(opts: &DaemonOptions) -> io::Result<i32> {
    let ghost = opts.root.join(GHOST_DIR);
    fs::create_dir_all(ghost.join("logs"))?;

    if let Some(existing) = read_pid(&opts.pid_path())? {
        if pid_alive(existing) {
            return Err(io::Error::other(format!(
                "daemon already running (pid {existing}); run `gt daemon stop` first"
            )));
        }
    }
    if opts.socket_path().exists() {
        // Dead daemon residue; a fresh bind after removal is the recovery path.
        let _ = fs::remove_file(opts.socket_path());
    }

    let exe = std::env::current_exe()?;
    let mut cmd = Command::new(&exe);
    cmd.args(["daemon", "start", "--foreground"])
        .current_dir(&opts.root)
        .stdin(std::process::Stdio::null())
        .process_group(0);
    if let Some(port) = opts.tcp_port {
        cmd.args(["--port", &port.to_string()]);
    }
    let stdout_log = fs::File::options()
        .create(true)
        .append(true)
        .open(opts.daemon_log_path())?;
    let stderr_log = fs::File::options()
        .create(true)
        .append(true)
        .open(opts.daemon_log_path())?;
    cmd.stdout(std::process::Stdio::from(stdout_log));
    cmd.stderr(std::process::Stdio::from(stderr_log));

    let mut child = cmd
        .spawn()
        .map_err(|e| io::Error::other(format!("failed to spawn {exe:?} daemon start: {e}")))?;
    let daemon_pid = child.id() as i32;

    // Wait until the socket appears (or the child dies). The child's pid file
    // content must equal its pid, guarding against pidfile races.
    let started = Instant::now();
    let ready = Duration::from_secs(10);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                return Err(io::Error::other(format!(
                    "daemon exited early (status: {status}); see {}",
                    opts.daemon_log_path().display()
                )));
            }
            Ok(None) => {}
            Err(e) => return Err(e),
        }
        if opts.socket_path().exists() && read_pid(&opts.pid_path())? == Some(daemon_pid) {
            break;
        }
        if started.elapsed() > ready {
            let _ = signal_pid(daemon_pid, libc::SIGKILL);
            return Err(io::Error::other(format!(
                "daemon did not become ready within {ready:?}; see {}",
                opts.daemon_log_path().display()
            )));
        }
        thread::sleep(Duration::from_millis(50));
    }

    // Detach: the CLI exits, the daemon continues orphaned (reaped by init).
    drop(child);
    Ok(daemon_pid)
}

/// Stop a running daemon: SIGTERM, bounded wait on clean pid-file removal,
/// `--force` escalates to SIGKILL plus manual artifact cleanup.
pub fn stop(opts: &DaemonOptions, force: bool) -> io::Result<()> {
    let pid_path = opts.pid_path();
    let socket_path = opts.socket_path();
    let mut cleaned_stale = false;

    match read_pid(&pid_path)? {
        None => {
            if socket_path.exists() {
                fs::remove_file(&socket_path)?;
                println!("[GhostTree] removed stale socket {}", socket_path.display());
                cleaned_stale = true;
            }
            if cleaned_stale {
                return Ok(());
            }
            Err(io::Error::other("no daemon running"))
        }
        Some(pid) => {
            if !pid_alive(pid) {
                let _ = fs::remove_file(&pid_path);
                if socket_path.exists() {
                    fs::remove_file(&socket_path)?;
                }
                println!("[GhostTree] cleaned stale daemon artifacts (pid {pid} is gone)");
                return Ok(());
            }
            let _ = signal_pid(pid, libc::SIGTERM);
            println!("[GhostTree] sent SIGTERM to daemon (pid {pid})");
            let deadline = Instant::now() + Duration::from_secs(10);
            while Instant::now() < deadline && pid_alive(pid) {
                // Clean shutdown removes the pid file before the process exits.
                if !pid_path.exists() {
                    return Ok(());
                }
                thread::sleep(Duration::from_millis(50));
            }
            if !pid_alive(pid) {
                let _ = fs::remove_file(&pid_path);
                let _ = fs::remove_file(&socket_path);
                return Ok(());
            }
            if force {
                signal_pid(pid, libc::SIGKILL)?;
                let kill_deadline = Instant::now() + Duration::from_secs(2);
                while Instant::now() < kill_deadline && pid_alive(pid) {
                    thread::sleep(Duration::from_millis(20));
                }
                let _ = fs::remove_file(&pid_path);
                let _ = fs::remove_file(&socket_path);
                println!("[GhostTree] killed daemon (pid {pid}) and cleaned artifacts");
                return Ok(());
            }
            Err(io::Error::other(format!(
                "daemon (pid {pid}) did not stop within 10s; use --force"
            )))
        }
    }
}
