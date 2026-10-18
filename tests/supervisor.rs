//! Integration tests for the PTY / process supervisor (SPEC §6, §8).

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use ghosttree::supervisor::{self, SpawnConfig};
use tempfile::tempdir;

const GRACE: Duration = Duration::from_millis(500);

fn config(command: &str, scratch: &Path, name: &str) -> SpawnConfig {
    let mut c = SpawnConfig::new(
        "testtask",
        name,
        command,
        scratch.to_path_buf(),
        scratch.join("logs"),
    );
    c.rows = 40;
    c.cols = 100;
    c
}

/// Read the raw log, polling briefly so pump flushes are visible.
fn read_log(path: &Path) -> String {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Ok(content) = fs::read_to_string(path) {
            return content;
        }
        assert!(
            Instant::now() < deadline,
            "log file never appeared: {path:?}"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(target_os = "linux")]
fn proc_count(needle: &str) -> Vec<i32> {
    let mut hits = Vec::new();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return hits;
    };
    for entry in entries.flatten() {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<i32>() else {
            continue;
        };
        if pid <= 0 {
            continue;
        }
        let Ok(cmdline) = fs::read(entry.path().join("cmdline")) else {
            continue;
        };
        // /proc cmdline is NUL-separated; compare against a space-joined view.
        let text = std::str::from_utf8(&cmdline).unwrap_or("");
        let joined = text.split('\0').collect::<Vec<_>>().join(" ");
        if joined.contains(needle) {
            hits.push(pid);
        }
    }
    hits
}

#[cfg(target_os = "linux")]
fn wait_for_procs(needle: &str, min: usize, timeout: Duration) -> Vec<i32> {
    let deadline = Instant::now() + timeout;
    loop {
        let hits = proc_count(needle);
        if hits.len() >= min {
            return hits;
        }
        assert!(Instant::now() < deadline, "processes never appeared");
        thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn captures_stdout_and_stderr_to_raw_log() {
    let dir = tempdir().unwrap();
    let c = config("echo out-marker; echo err-marker 1>&2", dir.path(), "c1");
    let expected_log = dir.path().join("logs/c1.raw.log");

    let spawned = supervisor::spawn(&c).unwrap();
    assert_eq!(
        spawned.pgid(),
        spawned.pid() as i32,
        "child is a group leader"
    );
    assert_eq!(spawned.log_path(), expected_log.as_path());
    let log_path: PathBuf = spawned.log_path().to_path_buf(); // wait() consumes the handle

    let outcome = spawned.wait().unwrap();
    assert!(outcome.success());
    let log = read_log(&log_path);
    assert!(log.contains("out-marker"), "log was: {log:?}");
    assert!(
        log.contains("err-marker"),
        "stderr must flow through the pty: {log:?}"
    );
    assert!(
        outcome.log_bytes >= log.len() as u64,
        "pump reported {} bytes but log file holds {} bytes",
        outcome.log_bytes,
        log.len()
    );
}

#[test]
fn child_sees_a_bona_fide_tty() {
    let dir = tempdir().unwrap();
    let c = config("test -t 0 && test -t 1 && test -t 2", dir.path(), "c2");
    let outcome = supervisor::spawn(&c).unwrap().wait().unwrap();
    assert!(
        outcome.success(),
        "child stdio must be a terminal (rc={:?})",
        outcome.exit_code()
    );
}

#[test]
fn tty_identity_and_window_size() {
    let dir = tempdir().unwrap();
    let c = config("tty", dir.path(), "c2b");
    let spawned = supervisor::spawn(&c).unwrap();
    let log_path: PathBuf = spawned.log_path().to_path_buf();

    // The configured geometry is visible from the master side.
    let fd = spawned.master_fd().expect("master fd open while running");
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    assert_eq!(unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut ws) }, 0);
    assert_eq!(ws.ws_row, 40);
    assert_eq!(ws.ws_col, 100);

    let outcome = spawned.wait().unwrap();
    assert!(outcome.success());
    let log = read_log(&log_path);
    assert!(log.trim().contains("/dev/pts/"), "tty reported: {log:?}");
}

#[test]
fn send_input_reaches_the_agent() {
    let dir = tempdir().unwrap();
    let c = config("read -r line; echo \"got:$line\"", dir.path(), "c4");
    let spawned = supervisor::spawn(&c).unwrap();
    let log_path: PathBuf = spawned.log_path().to_path_buf();

    let written = spawned.send_input(b"hello pty\n").unwrap();
    assert_eq!(written, "hello pty\n".len());

    let outcome = spawned.wait().unwrap();
    assert!(outcome.success());
    let log = read_log(&log_path);
    assert!(log.contains("got:hello pty"), "log was: {log:?}");
}

#[test]
fn injected_environment_reaches_the_worker() {
    let dir = tempdir().unwrap();
    let mut c = config("echo \"PORT=$PORT\"", dir.path(), "c5");
    let mut env = BTreeMap::new();
    env.insert("PORT".to_string(), "3107".to_string());
    c.env = env;
    let spawned = supervisor::spawn(&c).unwrap();
    let log_path: PathBuf = spawned.log_path().to_path_buf();
    let outcome = spawned.wait().unwrap();
    assert!(outcome.success());
    let log = read_log(&log_path);
    assert!(log.contains("PORT=3107"), "log was: {log:?}");
}

#[test]
fn wait_timeout_times_out_then_terminate_kills() {
    let dir = tempdir().unwrap();
    let c = config("sleep 30", dir.path(), "c6");
    let mut spawned = supervisor::spawn(&c).unwrap();

    let start = Instant::now();
    let still_running = spawned.wait_timeout(Duration::from_millis(300)).unwrap();
    assert!(still_running.is_none(), "sleep 30 cannot finish in 300ms");
    assert!(start.elapsed() < Duration::from_secs(5));

    let outcome = spawned.terminate(GRACE).unwrap();
    assert!(!outcome.success());
    // `sleep` has no TERM handler: it dies on the first signal.
    assert_eq!(outcome.signal(), Some(libc::SIGTERM));
}

#[test]
#[cfg(target_os = "linux")]
fn terminate_kills_the_entire_process_group() {
    let dir = tempdir().unwrap();
    // One shell, two backgrounded sleeps, plus `wait`: killing only the
    // direct child would orphan the sleeps; the group kill must reach them.
    let c = config("sleep 31 & sleep 31 & wait", dir.path(), "c7");
    let spawned = supervisor::spawn(&c).unwrap();

    // Other tests run `sleep 30` concurrently; a unique duration keeps the
    // /proc scan unambiguous. The `sh -c` wrapper also matches the needle,
    // hence the >= instead of a strict equality.
    let survivors = wait_for_procs("sleep 31", 2, Duration::from_secs(5));
    assert!(survivors.len() >= 2, "both sleeps must be running pre-kill");

    let start = Instant::now();
    let outcome = spawned.terminate(GRACE).unwrap();
    assert!(
        start.elapsed() < Duration::from_secs(5),
        "escalation was slow"
    );
    assert!(!outcome.success());

    let deadline = Instant::now() + Duration::from_secs(5);
    while !proc_count("sleep 31").is_empty() {
        assert!(Instant::now() < deadline, "sleep descendants leaked");
        thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn exit_code_is_reported() {
    let dir = tempdir().unwrap();
    let c = config("exit 7", dir.path(), "c8");
    let outcome = supervisor::spawn(&c).unwrap().wait().unwrap();
    assert_eq!(outcome.exit_code(), Some(7));
    assert!(!outcome.success());
}

#[test]
fn pump_completes_after_child_exit_and_counts_bytes() {
    // Deterministic successor of an "EIO on late write" check: the kernel is
    // free to accept writes into a dead pty's buffer, but the pump must still
    // terminate once the slave side closes and report the captured bytes.
    let dir = tempdir().unwrap();
    let c = config("printf 'alpha\\nbeta\\n'", dir.path(), "c9");
    let spawned = supervisor::spawn(&c).unwrap();
    let log_path = spawned.log_path().to_path_buf();

    let outcome = spawned.wait().unwrap();
    assert!(outcome.success());
    let content = fs::read(&log_path).unwrap();
    assert_eq!(content, b"alpha\nbeta\n");
    assert!(outcome.log_bytes >= content.len() as u64);
}
