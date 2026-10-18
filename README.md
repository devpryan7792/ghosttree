# GhostTree

**Autonomous Git worktree swarm orchestrator.**

Run *N* competing AI coding agents on the same task — each in its own isolated
Git worktree and PTY — grade the results on real test runs and diff statistics,
and merge the winner into your branch with one command.

[![CI](https://github.com/devpryan7792/ghosttree/actions/workflows/ci.yml/badge.svg)](https://github.com/devpryan7792/ghosttree/actions/workflows/ci.yml)
[![MSRV](https://img.shields.io/badge/rust-1.80%2B-blue)](https://www.rust-lang.org)
[![License](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue)](#license)

## Why

When autonomous agents (Claude Code, Aider, Cursor-style loops, custom LLM
workers) operate directly in your active working tree, three things go wrong:

1. **Branch pollution** — an agent rewriting files invalidates your editor
   buffers, LSP state, and staged changes.
2. **Sequential bottleneck** — exploring 3 architectural ideas takes
   `3 × T` back-to-back; running them in one directory causes write collisions.
3. **Destructive failures** — a hallucinating agent running `git checkout -- .`
   or `rm -rf` on *your* tree is a bad day.

GhostTree fixes this by giving every agent a throwaway worktree, supervising
them concurrently, and arbitrating the outcome.

## How it works

```
gt swarm "<prompt>" -n 3 --agents "<agent-cmds>" --test-cmd "cargo test"
```

1. The daemon provisions *N* worktrees (`ghosttree/<task>/<variant>` branches)
   using **git plumbing only** — never `checkout`/`pull`.
2. Each agent runs in its own **PTY and process group**, so tooling, compilers,
   and language servers can never orphan: kills are group-wide
   (`SIGTERM → SIGKILL` ladder).
3. Runners watch for commits, enforce an idle watchdog, then run your test
   command per candidate with a hard timeout.
4. The **arbitrator** scores every candidate and picks a winner
   automatically; the live dashboard shows it all in real time.
5. You promote with `gt merge` and clean up with `gt purge`.

## Quickstart

```bash
cargo install --path .          # installs the `gt` binary
gt init                         # provisions .ghosttree/ state in your repo

gt swarm "Fix the race condition in the connection pool" \
  -n 3 \
  --agents "claude-code --run, aider --yes, gemini --exec" \
  --test-cmd "cargo test"
```

You'll see the live dashboard (press `q` to detach — the swarm keeps running):

```
[GhostTree] Fix the race condition… — live dashboard (press 'q' to detach)

  ID       AGENT         STATUS     ELAPSED  COMMITS   TESTS   SCORE
  --------------------------------------------------------------------
  01JMW9…  claude-code   TESTING    01m 24s       2     8/8     96.4
  01JMW9…  aider         WORKING    01m 22s       5      --       --
  01JMW9…  gemini        FAILED     01m 22s       0     3/8     32.0
```

When arbitration picks a winner:

```
[GhostTree] Winner detected: 01JMW9E (c1, score 96.4) — task COMPLETED.
Run 'gt merge 01JMW9E' to promote changes into the active branch.
```

Then inspect, promote, and clean up:

```bash
gt diff c1            # unified diff vs. the task's base commit (or --stat)
gt eval <task-id>     # re-run the arbitrator (--re-run re-executes tests)
gt merge c1           # merge-tree dry-run first; refuses a dirty index
                      # (yes=--autostash snapshots via `git stash create`)
gt merge c1 --squash  # or squash the variant into one commit
gt purge              # remove worktrees, candidate branches, task rows
```

## Scoring (SPEC §7)

```
Score = 0.50·S_tests + 0.20·S_cov − 0.15·S_churn − 0.15·S_time
```

- **S_tests** — pass rate parsed from the harness output
  (cargo / pytest / jest / generic `N passed, N failed` formats).
- **S_cov** — delta coverage hook (inert by default; wire in a coverage tool).
- **S_churn** — lines changed per changed file: rewrites of unrelated files
  are penalized hard.
- **S_time** — execution duration against a 600 s normalizer.
- A run with **any failing test caps at 40** — a lucky low-churn candidate can
  never outrank a green one.

The winner is persisted on the task row; `gt eval --re-run` re-executes the
test command across candidates and re-arbitrates.

## CLI reference

| Command              | Purpose                                                     |
| -------------------- | ----------------------------------------------------------- |
| `gt init`            | Provision `.ghosttree/` state, logs, worktrees, config      |
| `gt list [--json]`   | Tasks, candidate counts, winners                            |
| `gt daemon start/st` | Control the supervisor daemon (UDS JSON-RPC + optional TCP) |
| `gt swarm`           | Spawn variants, watch the live dashboard, auto-arbitrate    |
| `gt diff <id>`       | Candidate diff vs. base commit (`--stat` for counters)      |
| `gt eval <task>`     | Trigger arbitrator re-evaluation (`--re-run`)               |
| `gt merge <id>`      | Promote a candidate branch (`--squash`, `--autostash`)      |
| `gt purge [task]`    | Remove worktrees + branches + task rows (`--all`)           |

All candidate references accept the full ULID, a unique 7-character prefix,
or a unique variant name (`c1`).

## Architecture

- **`gt` / `ghosttree`** — CLI binaries (clap).
- **Supervisor** — PTY + Unix process groups per candidate; deterministic
  termination ladder; raw logs per candidate.
- **Daemon** — JSON-RPC 2.0 over a Unix domain socket
  (`.ghosttree/ghosttree.sock`), optional extra TCP listener; newline-framed,
  per-connection single writers; `CandidateStateChanged` broadcasts.
- **Store** — SQLite in WAL mode (`PRAGMA foreign_keys = ON`,
  `busy_timeout = 5000`); the §3.1 schema is applied verbatim.
- **Arbitrator** — test-output parsing, `git diff --numstat` statistics,
  multi-criteria scoring, winner selection.
- **Dashboard** — crossterm TUI with a plain streaming fallback.

### Safety properties

- **Plumbing-only git** — `merge-tree --write-tree` dry-runs every merge;
  nothing is checked out inside product code.
- **Orphan-proof** — every agent and test run is a session leader; kills
  traverse the whole process group.
- **Boot reconciliation** — orphaned candidates become FAILED, dead tasks
  ABORTED; stale sockets and pid files recover automatically.
- **Purge guards** — refuses to purge a task whose candidates are still live.

## Development

```bash
cargo test                                       # 99 tests, unit + integration
cargo clippy --all-targets -- -D warnings        # zero-warning policy
cargo fmt --check
```

Rust **1.80+** (MSRV), Linux targeted per the [SPECIFICATION.md](SPECIFICATION.md);
the daemon, IPC, and schema sections there are authoritative.

## License

Dual-licensed under either of:

- Apache License 2.0 — [LICENSE-APACHE](LICENSE-APACHE)
- MIT license — [LICENSE-MIT](LICENSE-MIT)

at your option.
