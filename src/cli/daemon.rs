//! `gt daemon start|stop` (SPEC §5.1): lifecycle control of the supervisor
//! daemon that owns the JSON-RPC socket and the candidate runners.

use std::path::Path;

use anyhow::{bail, Context, Result};

use crate::cli::DaemonAction;
use crate::daemon::{self, DaemonOptions};

/// Resolve the repo + check `gt init` ran here, like the other subcommands.
fn daemon_options() -> Result<DaemonOptions> {
    let repo = crate::git::Repo::discover(Path::new("."))
        .context("gt daemon must run inside a git repository")?;
    let db = repo.root().join(".ghosttree").join("ghosttree.db");
    if !db.exists() {
        bail!(
            "GhostTree is not initialized here (missing {}); run `gt init` first",
            db.display()
        );
    }
    Ok(DaemonOptions::new(repo.root().to_path_buf()))
}

pub(crate) fn dispatch(action: DaemonAction) -> Result<()> {
    match action {
        DaemonAction::Start { foreground, port } => start(foreground, port),
        DaemonAction::Stop { force } => stop(force),
    }
}

fn start(foreground: bool, port: Option<u16>) -> Result<()> {
    let mut opts = daemon_options()?;
    opts.tcp_port = port;
    if foreground {
        daemon::serve(&opts).map_err(|e| anyhow::anyhow!("daemon exited: {e}"))
    } else {
        let pid = daemon::start_background(&opts)
            .map_err(|e| anyhow::anyhow!("failed to start daemon: {e}"))?;
        println!("GhostTree daemon started (pid {pid})");
        println!("  socket: {}", opts.socket_path().display());
        println!("  log:    {}", opts.daemon_log_path().display());
        Ok(())
    }
}

fn stop(force: bool) -> Result<()> {
    let opts = daemon_options()?;
    daemon::stop(&opts, force).map_err(|e| anyhow::anyhow!("{e}"))
}
