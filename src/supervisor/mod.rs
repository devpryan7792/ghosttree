//! Process & PTY supervisor (SPEC §6).
//!
//! Phase 2 public API: spawn a candidate agent inside its own pseudo-terminal
//! and process group, stream its output into the per-task raw log, and
//! terminate it deterministically (SIGTERM -> grace -> SIGKILL over the whole
//! process group). The Phase 3 daemon will call this from its state machine.

pub(crate) mod proc;
pub(crate) mod pty;

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{self, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

/// Default terminal geometry (SPEC dashboards render 80 columns).
pub const DEFAULT_ROWS: u16 = 24;
pub const DEFAULT_COLS: u16 = 80;

/// Grace-period escalation ceiling before SIGKILL is sent to the group (SPEC §8).
pub const TERMINATION_GRACE: Duration = Duration::from_secs(5);

/// How long `wait`/`terminate` wait for the log pump to report completion
/// before detaching it (orphans holding the slave fd keep the read open).
const LOG_JOIN_TIMEOUT: Duration = Duration::from_millis(2_000);

/// Polling granularity for `wait_timeout`.
const POLL_INTERVAL: Duration = Duration::from_millis(10);

/// Everything needed to launch one worker candidate.
#[derive(Debug, Clone)]
pub struct SpawnConfig {
    /// Owning task id (used for the log thread name and path layout).
    pub task_id: String,
    /// Candidate id; the log file is `<log_dir>/<candidate_id>.raw.log`.
    pub candidate_id: String,
    /// Full agent command line, executed as `sh -c <command>`.
    pub command: String,
    /// Working directory (the ghost worktree root).
    pub cwd: PathBuf,
    /// Extra environment injected into the worker (PORT, DATABASE_URL, ...).
    pub env: BTreeMap<String, String>,
    /// Directory for this task's raw capture logs (`.ghosttree/logs/<task-id>`).
    pub log_dir: PathBuf,
    pub rows: u16,
    pub cols: u16,
}

impl SpawnConfig {
    pub fn new(
        task_id: impl Into<String>,
        candidate_id: impl Into<String>,
        command: impl Into<String>,
        cwd: PathBuf,
        log_dir: PathBuf,
    ) -> Self {
        Self {
            task_id: task_id.into(),
            candidate_id: candidate_id.into(),
            command: command.into(),
            cwd,
            env: BTreeMap::new(),
            log_dir,
            rows: DEFAULT_ROWS,
            cols: DEFAULT_COLS,
        }
    }
}

/// Result of a completed candidate run.
#[derive(Debug, Clone)]
pub struct ExitOutcome {
    pub status: ExitStatus,
    /// Bytes appended to the raw log by the pump thread (0 if it was still
    /// blocked on an orphan when the wait completed).
    pub log_bytes: u64,
}

impl ExitOutcome {
    pub fn success(&self) -> bool {
        self.status.success()
    }

    /// Process exit code, if it exited normally.
    pub fn exit_code(&self) -> Option<i32> {
        self.status.code()
    }

    /// Signal that terminated the process, if any (0/None otherwise).
    pub fn signal(&self) -> Option<i32> {
        self.status.signal()
    }
}

/// A live candidate worker: pty master side, process group, and log pump.
pub struct Spawned {
    group: proc::ProcessGroup,
    /// The master fd kept for `send_input`; `None` once closed after exit.
    master: Arc<Mutex<Option<File>>>,
    log_rx: Receiver<io::Result<u64>>,
    log_path: PathBuf,
}

/// Spawn a candidate agent into its own PTY and process group.
///
/// On success the caller owns a [`Spawned`] handle; the log pump runs on a
/// detached thread until the child exits (`spawn_log_pump` then reports via
/// the receiver carried inside `Spawned`).
pub fn spawn(cfg: &SpawnConfig) -> io::Result<Spawned> {
    fs::create_dir_all(&cfg.log_dir)?;
    let log_path = cfg.log_dir.join(format!("{}.raw.log", cfg.candidate_id));

    let pair = pty::open_pty(cfg.rows, cfg.cols)?;
    let master_raw = pair.master.as_raw_fd();
    let slave_raw = pair.slave.as_raw_fd();
    // The pump thread reads a dup of the master so that `send_input` can keep
    // the write side open in the supervisor without racing on one fd owner.
    let reader = pair.master.try_clone()?;

    let mut cmd = std::process::Command::new("sh");
    cmd.arg("-c")
        .arg(&cfg.command)
        .current_dir(&cfg.cwd)
        // Agents expect a TERM matching a real terminal session (SPEC §6.1).
        .env("TERM", "xterm-256color");
    for (key, value) in &cfg.env {
        cmd.env(key, value);
    }
    pty::configure_child_tty(&mut cmd, master_raw, slave_raw);

    let group = proc::ProcessGroup::spawn(cmd)?;
    debug_assert_eq!(
        group.pgid(),
        group.pid() as i32,
        "setsid must make the child a group leader"
    );

    // Parent-side slave must be closed so master reads reach EIO on child exit.
    drop(pair.slave);

    let log_rx = pty::spawn_log_pump(&cfg.task_id, reader, log_path.clone())?;

    Ok(Spawned {
        group,
        master: Arc::new(Mutex::new(Some(File::from(pair.master)))),
        log_rx,
        log_path,
    })
}

impl Spawned {
    /// Child pid (== process group id).
    pub fn pid(&self) -> u32 {
        self.group.pid()
    }

    /// Process group id; the liveness scope for `kill(-pgid, ...)`.
    pub fn pgid(&self) -> i32 {
        self.group.pgid()
    }

    /// Path of the raw capture log for this candidate.
    pub fn log_path(&self) -> &Path {
        &self.log_path
    }

    /// Current master fd for out-of-band ioctls (e.g. TIOCGWINSZ), if open.
    pub fn master_fd(&self) -> Option<RawFd> {
        self.master
            .lock()
            .expect("pty master mutex poisoned")
            .as_ref()
            .map(AsRawFd::as_raw_fd)
    }

    /// Write keystrokes/commands into the agent's terminal (`gt attach` input
    /// path). Errors once the pty is closed after process exit.
    pub fn send_input(&self, data: &[u8]) -> io::Result<usize> {
        let mut slot = self.master.lock().expect("pty master mutex poisoned");
        let fd = slot
            .as_mut()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotConnected, "pty already closed"))?;
        fd.write(data)
    }

    /// Non-blocking status poll.
    pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        self.group.try_wait()
    }

    /// Poll until `timeout`; `None` means the candidate is still running and
    /// the caller should escalate (`terminate`).
    pub fn wait_timeout(&mut self, timeout: Duration) -> io::Result<Option<ExitStatus>> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(status) = self.group.try_wait()? {
                return Ok(Some(status));
            }
            if Instant::now() >= deadline {
                return Ok(None);
            }
            thread::sleep(POLL_INTERVAL);
        }
    }

    /// Block until the candidate exits, then close the pty and collect the
    /// pump result. Use `wait_timeout` + `terminate` for bounded waits.
    pub fn wait(mut self) -> io::Result<ExitOutcome> {
        let status = self.group.wait()?;
        self.collect(status)
    }

    /// Finish tracking a candidate whose group leader was already reaped via
    /// `try_wait` (the daemon runner polls rather than blocks): closes the
    /// pty and collects the pump result.
    pub fn reap(self, status: ExitStatus) -> io::Result<ExitOutcome> {
        self.collect(status)
    }

    /// Terminate the whole process group (SPEC §8: SIGTERM, `grace`, SIGKILL),
    /// then reap and collect.
    pub fn terminate(mut self, grace: Duration) -> io::Result<ExitOutcome> {
        let status = self.group.terminate(grace)?;
        self.collect(status)
    }

    fn collect(&self, status: ExitStatus) -> io::Result<ExitOutcome> {
        // Closing our write handle signals the end of input; the pump exits on
        // its own EIO once the child (and any orphan slaves) are gone.
        if let Some(master) = self
            .master
            .lock()
            .expect("pty master mutex poisoned")
            .take()
        {
            drop(master);
        }
        let log_bytes = match self.log_rx.recv_timeout(LOG_JOIN_TIMEOUT) {
            Ok(Ok(n)) => n,
            Ok(Err(e)) => return Err(e),
            // Pump still blocked on an orphan grandchild holding the slave:
            // detach it; the bytes are already flushed incrementally.
            Err(_) => 0,
        };
        Ok(ExitOutcome { status, log_bytes })
    }
}
