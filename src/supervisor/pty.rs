//! PTY allocation and child wiring (SPEC §6.1).
//!
//! Each candidate agent runs attached to a freshly allocated pseudo-terminal
//! so it sees a real TTY (`TERM`, `test -t`, ANSI output, terminal dimensions).
//! The daemon keeps the master side; a pump thread streams master output to
//! the on-disk raw log.

use std::fs::{File, OpenOptions};
use std::io::{self, BufWriter, Read, Write};
use std::os::fd::{OwnedFd, RawFd};
use std::path::PathBuf;
use std::process::Command;
use std::sync::mpsc::{channel, Receiver};
use std::thread;

use nix::pty::openpty;
use nix::sys::termios::{cfmakeraw, tcgetattr, tcsetattr, SetArg, Termios};

/// A master/slave pseudo-terminal pair. The slave is closed in the parent
/// immediately after spawning; the master stays with the supervisor.
pub(crate) struct PtyPair {
    pub master: OwnedFd,
    pub slave: OwnedFd,
}

/// Open a pty pair with the requested window size and put the master into
/// raw mode so log captures contain bytes exactly as the agent wrote them
/// (no ONLCR translation, no line-discipline cooking).
pub(crate) fn open_pty(rows: u16, cols: u16) -> Result<PtyPair, io::Error> {
    let winsize = nix::pty::Winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // nix 0.31 signature: openpty(winsize, termios), each Into<Option<&T>>.
    let pair = openpty(Some(&winsize), None::<&Termios>).map_err(nix_error)?;
    // Raw mode on the master descriptor controls the pty's line discipline,
    // so captured bytes are exactly what the agent wrote (no ONLCR cooking).
    let mut tios = tcgetattr(&pair.master).map_err(nix_error)?;
    cfmakeraw(&mut tios);
    tcsetattr(&pair.master, SetArg::TCSANOW, &tios).map_err(nix_error)?;
    Ok(PtyPair {
        master: pair.master,
        slave: pair.slave,
    })
}

/// Register the `pre_exec` hook that turns a plain child process into a
/// session leader with `slave_fd` as its controlling terminal (SPEC §6.1:
/// PTY master/slave + process groups via `setsid`).
pub(crate) fn configure_child_tty(cmd: &mut Command, master_fd: RawFd, slave_fd: RawFd) {
    use std::os::unix::process::CommandExt;
    unsafe {
        cmd.pre_exec(move || {
            // setsid(): new session + new process group; the child becomes a
            // group leader, so pgid == pid and kill(-pgid, ...) works.
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            // Acquire the controlling terminal (allowed right after setsid).
            if libc::ioctl(slave_fd, libc::TIOCSCTTY as libc::c_ulong, 0) == -1 {
                return Err(io::Error::last_os_error());
            }
            for fd in [0, 1, 2] {
                if libc::dup2(slave_fd, fd) == -1 {
                    return Err(io::Error::last_os_error());
                }
            }
            // Neither pty end may leak into the child's file descriptors.
            if slave_fd > 2 {
                libc::close(slave_fd);
            }
            libc::close(master_fd);
            Ok(())
        });
    }
}

/// Spawn the log-pump thread for a candidate and return a receiver carrying
/// the pump result (total bytes appended, or the io error that ended it).
///
/// The receiver lets `Spawned::wait` join the thread with a timeout; the
/// thread self-terminates when every slave fd is closed (child exit), which
/// makes a blocking master read fail with `EIO`.
pub(crate) fn spawn_log_pump(
    task_id: &str,
    reader: OwnedFd,
    log_path: PathBuf,
) -> io::Result<Receiver<io::Result<u64>>> {
    let (tx, rx) = channel();
    thread::Builder::new()
        .name(format!("gt-log-{task_id}"))
        .spawn(move || {
            let result = pump_to_log(reader, &log_path);
            let _ = tx.send(result);
        })
        .map(|_| rx) // JoinHandle dropped: the channel reports completion instead
}

/// Drain `reader` into `log_path`, flushing after every chunk so dashboards
/// and `gt attach` see near-real-time output. Stops at EOF or `EIO`
/// (Linux/macOS signal that all slave descriptors are closed).
fn pump_to_log(reader: OwnedFd, log_path: &PathBuf) -> io::Result<u64> {
    let log_file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)?;
    let mut log = BufWriter::new(log_file);
    let mut src = File::from(reader);
    let mut total: u64 = 0;
    let mut buf = [0u8; 8192];
    loop {
        match src.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                log.write_all(&buf[..n])?;
                log.flush()?;
                total += n as u64;
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) if e.raw_os_error() == Some(libc::EIO) => break,
            Err(e) => return Err(e),
        }
    }
    log.flush()?;
    Ok(total)
}

fn nix_error(err: nix::Error) -> io::Error {
    io::Error::from(err)
}
