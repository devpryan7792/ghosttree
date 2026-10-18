//! GhostTree: autonomous Git worktree swarm orchestrator.
//!
//! Library crate shared by the `gt` and `ghosttree` binaries.

pub mod arbitrator;
pub mod cli;
pub mod daemon;
pub mod db;
pub mod git;
pub mod supervisor;
pub mod ui;
