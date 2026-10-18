//! Process group management and clean termination (SPEC §6.1, §8).
//!
//! Candidate agents run as session leaders (their own process group), so all
//! descendant processes — compilers, language servers, test runners — are
//! killed together with `kill(-pgid, SIG*)`, leaving no orphans behind.

use std::io;
use std::process::{Child, Command, ExitStatus};
use std::thread;
use std::time::{Duration, Instant};

/// Polling granularity for wait loops and signal escalation.
const POLL_INTERVAL: Duration = Duration::from_millis(10);

/// A child process that is a process-group leader (see
/// `pty::configure_child_tty`): `pgid == pid`.
pub(crate) struct ProcessGroup {
    child: Child,
    pid: u32,
}

impl ProcessGroup {
    /// Spawn `cmd`. The pre-registered `pre_exec` hook is expected to have
    /// called `setsid()`, making the child its own group leader.
    pub fn spawn(mut cmd: Command) -> io::Result<Self> {
        let child = cmd.spawn()?;
        let pid = child.id();
        Ok(Self { child, pid })
    }

    /// The child's pid.
    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// Process group id — equals `pid` because the child ran `setsid()`.
    pub fn pgid(&self) -> i32 {
        self.pid as i32
    }

    /// Non-blocking reap: `None` while the process is still running.
    pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        self.child.try_wait()
    }

    /// Blocking reap.
    pub fn wait(&mut self) -> io::Result<ExitStatus> {
        self.child.wait()
    }

    /// Signal every member of the process group (positive pgid argument
    /// negated into `kill(-pgid, sig)`, per SPEC §6.1).
    fn signal_group(&self, sig: i32) -> io::Result<()> {
        // SAFETY: raw syscall wrapper; only errno handling is unsafe-adjacent.
        let rc = unsafe { libc::kill(-self.pgid(), sig) };
        if rc == 0 {
            return Ok(());
        }
        let err = io::Error::last_os_error();
        match err.raw_os_error() {
            // The group already terminated (or was reaped): nothing to do.
            Some(libc::ESRCH) => Ok(()),
            _ => Err(err),
        }
    }

    /// Terminate the group gracefully: SIGTERM, then poll for `grace`, then
    /// SIGKILL, then reap the direct child (SPEC §8 escalation ladder).
    pub fn terminate(&mut self, grace: Duration) -> io::Result<ExitStatus> {
        self.signal_group(libc::SIGTERM)?;
        let deadline = Instant::now() + grace;
        loop {
            if let Some(status) = self.child.try_wait()? {
                return Ok(status);
            }
            if Instant::now() >= deadline {
                break;
            }
            thread::sleep(POLL_INTERVAL);
        }
        self.signal_group(libc::SIGKILL)?;
        self.child.wait()
    }
}
