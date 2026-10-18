# GhostTree: Autonomous Git Worktree Swarm Orchestrator
## Engineering Specification Document (v1.0.0-PROD)
**Author:** Antigravity Systems Architecture Group  
**Target Architecture:** Linux (x86_64 / aarch64), macOS (Darwin arm64)  
**Primary Language Target:** Rust (MSRV 1.80+) or Go (1.23+)  
**Status:** Approved for Implementation  

---

## 1. Executive Summary & Problem Formulation

### 1.1 The Problem
When autonomous AI agents (such as Claude Code, Cursor, Aider, Devin, or custom LLM loops) operate on a software repository, they directly mutate the developer's active working tree. This creates catastrophic friction:
1. **Branch Pollution & Head Contention:** An agent rewriting files invalidates the developer's active LSP state, editor buffers, and uncommitted staged changes.
2. **Sequential Bottleneck:** If a user wants to explore 3 distinct architectural approaches to an issue, running them sequentially takes $3 \times T$. Running them concurrently on the same directory causes non-deterministic file write collisions.
3. **Destructive Failures:** A hallucinating agent running `git checkout -- .` or `rm -rf` in an active workspace can permanently destroy uncommitted developer work.
4. **Merge Hell:** Manually reconciling conflicting patches from two autonomous runs requires manual three-way diff inspection without execution context.

### 1.2 The Solution: GhostTree
`GhostTree` is a high-performance local daemon and CLI tool that provisions ephemeral, isolated Git worktrees (`.ghosttree/worktrees/<task-id>-<variant>`), dispatches autonomous worker sessions into isolated virtual environments with pty/process tracking, runs deterministic test suites against each variant, benchmarks their resource consumption and test passes, and executes an automated AST-aware patch reconciliation engine to promote the winning candidate back to the primary branch.

```
                             +------------------------+
                             |    Developer CLI /     |
                             |   ghosttree dispatch   |
                             +-----------+------------+
                                         |
                                         v
                             +------------------------+
                             |   GhostTree Daemon     |
                             | (State Machine/SQLite) |
                             +----+------+-------+----+
                                  |      |       |
            +---------------------+      |       +---------------------+
            |                            |                             |
            v                            v                             v
+-----------------------+    +-----------------------+    +-----------------------+
|  Worktree Alpha       |    |  Worktree Beta        |    |  Worktree Gamma       |
|  (.ghosttree/wt/01)   |    |  (.ghosttree/wt/02)   |    |  (.ghosttree/wt/03)   |
|  Agent: Claude 3.7    |    |  Agent: DeepSeek R1   |    |  Agent: Local Qwen    |
|  Strategy: Refactor   |    |  Strategy: Quick-Fix  |    |  Strategy: TDD First  |
+-----------+-----------+    +-----------+-----------+    +-----------+-----------+
            |                            |                             |
            | (Runs Tests / Coverage)   | (Runs Tests / Coverage)     | (Runs Tests / Coverage)
            +---------------------+      |       +---------------------+
                                  |      |       |
                                  v      v       v
                             +------------------------+
                             | Arbitrator & Test Rank |
                             |   (Candidate Matrix)   |
                             +-----------+------------+
                                         |
                                         v
                             +------------------------+
                             | Selective Merge / Rebase|
                             | -> main working tree   |
                             +------------------------+
```

---

## 2. Terminology & System Constants

| Term | Definition |
| :--- | :--- |
| **Primary Repository (PR)** | The root git repository where the user invoked `ghosttree init`. Located at `/path/to/repo`. |
| **Ghost Worktree (GW)** | An isolated Git worktree created via `git worktree add --detach` inside the `.ghosttree/worktrees/` directory. |
| **Swarm Session** | A logical grouping of 1 to $N$ competing or complementary agent runs addressing a single Prompt / Issue ID. |
| **Worker Candidate** | A specific worktree running a specific agent command and strategy. |
| **Arbitrator** | The deterministic evaluation harness that executes test commands, linters, and coverage scripts inside each worktree to score candidates. |
| **Ghost Lock** | An atomic filesystem lock preventing simultaneous mutation of shared git references (`refs/ghosttree/*`). |

### 2.1 File System Hierarchy
```
<repo-root>/
├── .git/
├── .ghosttree/
│   ├── ghosttree.db            # SQLite WAL state store (tasks, runs, logs, metrics)
│   ├── ghosttree.sock          # Unix domain socket for daemon IPC
│   ├── daemon.pid              # Process ID of active daemon
│   ├── config.toml             # Repository-specific configuration
│   ├── logs/
│   │   ├── daemon.log          # Daemon system logs
│   │   └── <task-id>/          # Per-task raw stdout/stderr/pty captures
│   └── worktrees/
│       ├── wt-a8f10b-candidate-1/
│       ├── wt-a8f10b-candidate-2/
│       └── wt-a8f10b-candidate-3/
```

---

## 3. Core Data Structures & State Machine

### 3.1 SQLite Relational Schema (`ghosttree.db`)

All state transitions are persisted via SQLite with `PRAGMA journal_mode = WAL;` and `PRAGMA synchronous = NORMAL;`.

```sql
CREATE TABLE IF NOT EXISTS tasks (
    id TEXT PRIMARY KEY,               -- UUIDv4 / short ulid
    title TEXT NOT NULL,
    prompt TEXT NOT NULL,
    base_commit TEXT NOT NULL,         -- SHA-1 or SHA-256 commit hash
    base_branch TEXT NOT NULL,
    strategy TEXT NOT NULL,            -- 'competing' | 'decomposed' | 'speculative'
    status TEXT NOT NULL,              -- 'PENDING'|'RUNNING'|'EVALUATING'|'COMPLETED'|'ABORTED'
    winner_candidate_id TEXT,
    created_at INTEGER NOT NULL,       -- Epoch milliseconds
    updated_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS candidates (
    id TEXT PRIMARY KEY,
    task_id TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
    variant_name TEXT NOT NULL,        -- e.g. "sonnet-minimal", "deepseek-rewrite"
    worktree_path TEXT NOT NULL,       -- Absolute path
    branch_name TEXT NOT NULL,         -- "ghosttree/task-id/variant-name"
    agent_cmd TEXT NOT NULL,           -- Executable string
    pid INTEGER,
    status TEXT NOT NULL,              -- 'INITIALIZING'|'WORKING'|'TESTING'|'READY'|'FAILED'|'MERGED'
    head_commit TEXT,                  -- Resulting commit SHA
    exit_code INTEGER,
    score REAL DEFAULT 0.0,
    test_results_json TEXT,            -- { "passed": 12, "failed": 0, "duration_ms": 420 }
    diff_stat_json TEXT,               -- { "files_changed": 4, "insertions": 110, "deletions": 12 }
    started_at INTEGER,
    finished_at INTEGER
);

CREATE TABLE IF NOT EXISTS candidate_events (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    candidate_id TEXT NOT NULL REFERENCES candidates(id) ON DELETE CASCADE,
    event_type TEXT NOT NULL,          -- 'SYSCALL'|'GIT_COMMIT'|'TEST_RUN'|'AGENT_STDOUT'
    payload TEXT NOT NULL,
    timestamp INTEGER NOT NULL
);
```

### 3.2 Candidate Lifecycle State Machine

```
               +--------------+
               | INITIALIZING |
               +-------+------+
                       | (Worktree created, base branched)
                       v
               +--------------+
               |   WORKING    | <----+
               +-------+------+      | (Agent active, commits recorded)
                       |             |
         Agent finishes|             |
                       v             |
               +--------------+      |
               |   TESTING    +------+ (Retry if test failed and budget remains)
               +-------+------+
                       |
         +-------------+-------------+
         | (Tests Pass)              | (Tests Fail / Timeout)
         v                           v
   +-----------+               +-----------+
   |   READY   |               |  FAILED   |
   +-----+-----+               +-----------+
         |
         | (Promoted by user or auto-arbitrator)
         v
   +-----------+
   |  MERGED   |
   +-----------+
```

---

## 4. Git Plumbing Integration Specification

Direct `git` CLI shell commands must never use high-level interactive commands like `git checkout` or `git pull`. `GhostTree` interacts solely through low-overhead Git plumbing:

### 4.1 Worktree Provisioning Engine
```bash
# 1. Fetch current repository root and base commit SHA
git rev-parse --show-toplevel
git rev-parse HEAD

# 2. Verify git worktree capability
git worktree list --porcelain

# 3. Create isolated branch off the current base SHA
git branch ghosttree/<task-id>/<variant> <base_sha>

# 4. Atomically create worktree in detached or specific branch mode
git worktree add \
    --no-checkout \
    -b ghosttree/<task-id>/<variant> \
    .ghosttree/worktrees/<task-id>-<variant> \
    <base_sha>

# 5. Populate worktree index cleanly
git -C .ghosttree/worktrees/<task-id>-<variant> reset --hard <base_sha>
```

### 4.2 Environmental Isolation in Worktree
To prevent agents from modifying global toolchains or node modules across worktrees:
* If `package.json` exists: Auto-create copy-on-write `node_modules` via hardlinks: `cp -al ../../../node_modules ./node_modules` (or trigger `pnpm install --prefer-offline`).
* If `Cargo.toml` exists: Set `CARGO_TARGET_DIR=.ghosttree/cache/target/<variant>` to avoid locking `target/debug`.
* If `.venv` exists: Generate a fast symlink/copy virtual environment with isolated `PYTHONPATH`.

### 4.3 Safe Patch Export & Merge Engine
When a candidate wins:
```bash
# Calculate clean patch set
git -C .ghosttree/worktrees/<task-id>-<winner> diff <base_sha>..HEAD > /tmp/winning_patch.diff

# Dry-run merge into primary tree
git merge-tree --write-tree <base_sha> HEAD <winner_head_sha>

# Apply without dirtying uncommitted work, or perform fast-forward rebase
git merge --no-ff -m "ghosttree(task): merge variant <variant> for <task-id>" ghosttree/<task-id>/<winner>
```

---

## 5. CLI Surface & Subcommands

The CLI binary is named `gt` (with alias `ghosttree`).

### 5.1 Command Specification Table

| Command | Arguments | Flags | Description |
| :--- | :--- | :--- | :--- |
| `gt init` | None | `--force` | Initializes `.ghosttree` in current git repo, builds DB, adds `.ghosttree` to `.gitignore`. |
| `gt daemon start` | None | `--foreground`, `--port <n>` | Starts the background supervisor daemon. |
| `gt daemon stop` | None | `--force` | Sends `SIGTERM` to daemon, cleans stale sockets. |
| `gt swarm` | `<prompt>` | `-n <variants>`, `--agents <list>`, `--test-cmd <cmd>` | Spawns $N$ competing worktrees with specified agent runners. |
| `gt list` | None | `--json`, `--all` | Displays interactive table of active tasks, candidates, and statuses. |
| `gt diff` | `<candidate-id>` | `--stat`, `--gui` | Shows git diff of a candidate against base commit. |
| `gt eval` | `<task-id>` | `--re-run` | Runs the test arbitrator across all ready candidates for a task. |
| `gt merge` | `<candidate-id>` | `--squash`, `--strategy <ast\|git>` | Merges the candidate's commits into active working branch. |
| `gt purge` | `[task-id]` | `--all`, `--dry-run` | Removes worktrees and prunes git references safely. |

### 5.2 Example CLI Invocation & Output Flow
```bash
$ gt swarm "Fix race condition in connection_pool.rs and add stress test" \
    --variants 3 \
    --agents "claude-code,aider,gemini" \
    --test-cmd "cargo test --test pool_stress"

[GhostTree] Task initialized: task_01JMW8K (Base: 7b31cf2 on main)
  ├── Candidate 1: claude-code -> .ghosttree/worktrees/task_01JMW8K-c1
  ├── Candidate 2: aider       -> .ghosttree/worktrees/task_01JMW8K-c2
  └── Candidate 3: gemini      -> .ghosttree/worktrees/task_01JMW8K-c3

[GhostTree] Spawning workers in pseudo-terminals...
[Candidate 1] Started (PID: 41920)
[Candidate 2] Started (PID: 41921)
[Candidate 3] Started (PID: 41922)

[Live Dashboard - Press 'q' to exit]
ID     Agent        Status     Elapsed   Commits  Tests Passed  Score
---------------------------------------------------------------------
c1     claude-code  TESTING    01m 24s   2        8/8           96.4
c2     aider        WORKING    01m 24s   1        --            --
c3     gemini       FAILED     00m 52s   1        3/8 (Err)     32.0

[GhostTree] Winner detected: Candidate 1 (Score: 96.4).
Run 'gt merge c1' to promote changes into main branch.
```

---

## 6. Process Supervision, PTY, & IPC Engine

### 6.1 Process Isolation
Each agent command must not be run via raw `std::process::Command` without a PTY. Agents expect interactive TTY signals (ANSI colors, terminal dimensions, raw keystroke hooks).
* **PTY Master/Slave:** The daemon allocates a pseudo-terminal pair (`openpty`) per candidate.
* **Stream Multiplexer:** The master fd streams output directly to:
  1. An on-disk raw log file: `.ghosttree/logs/<task-id>/<candidate-id>.raw.log`.
  2. The Unix domain socket for real-time terminal attachment (`gt attach <candidate-id>`).
* **Process Groups:** Every spawned candidate runs in its own process group (`setpgid(0, 0)`). When a candidate fails, times out, or is cancelled, `kill(-pgid, SIGKILL)` terminates all descendant subprocesses (compilers, language servers, test runners) without leaking orphan processes.

### 6.2 IPC Protocol (`ghosttree.sock`)
JSON-RPC 2.0 over Unix Domain Socket.
```json
// Request
{
  "jsonrpc": "2.0",
  "id": 1,
  "method": "SpawnSwarm",
  "params": {
    "task_id": "01JMW8K",
    "prompt": "Fix memory leak in buffer",
    "variants": [
      { "name": "c1", "agent_cmd": "claude-code --dangerously-skip-permissions" },
      { "name": "c2", "agent_cmd": "aider --yes-always" }
    ],
    "test_cmd": "cargo test"
  }
}

// Notification (Broadcast from Daemon)
{
  "jsonrpc": "2.0",
  "method": "CandidateStateChanged",
  "params": {
    "candidate_id": "c1",
    "status": "TESTING",
    "head_commit": "e4f810a",
    "timestamp": 1729245100120
  }
}
```

---

## 7. The Evaluation & Arbitrator Engine

To objectively pick the winner among competing agent solutions, `GhostTree` executes an automated scoring matrix:

$$\text{Final Score} = w_1 \cdot S_{\text{tests}} + w_2 \cdot S_{\text{cov}} - w_3 \cdot S_{\text{churn}} - w_4 \cdot S_{\text{time}}$$

Where:
* $S_{\text{tests}} = \frac{\text{Passed Tests}}{\text{Total Tests}} \times 100$ (Weight: $0.50$). If any test fails, score is capped at $40$.
* $S_{\text{cov}} = \text{Delta Test Coverage } \Delta\%$ (Weight: $0.20$).
* $S_{\text{churn}} = \frac{\text{Lines Added} + \text{Lines Deleted}}{\text{Baseline Files Changed}}$ (Weight: $0.15$). Heavily penalizes hallucinations that rewrite entire unrelated files.
* $S_{\text{time}} = \frac{\text{Execution Duration (seconds)}}{600}$ (Weight: $0.15$).

---

## 8. Failure Modes & Edge Case Recovery

| Failure Mode | Detection Mechanism | Mitigation Strategy |
| :--- | :--- | :--- |
| **Orphan Worktrees** | Daemon crash / reboot leaving stale directories in `.ghosttree/worktrees/`. | On daemon boot: run `git worktree prune`. Reconcile filesystem against `candidates` table in SQLite; mark unclosed candidates as `ABORTED`. |
| **Agent Infinite Loop** | Candidate process runtime $> \text{timeout}$ (default: 15 mins) with no git commits. | Send `SIGTERM` to candidate process group; wait 5s; send `SIGKILL`. Mark status `FAILED (TIMEOUT)`. |
| **Port / Resource Collision** | Two worktrees running dev servers binding to port 3000. | Inject dynamic environment variables into worker PTY: `PORT=3100 + candidate_index`, `DATABASE_URL=..._test_<variant>`. |
| **Dirty Index in Primary** | User has uncommitted changes when calling `gt merge`. | Refuse merge unless `--autostash` is passed. Use `git stash create` to store dangling blob safely before applying. |

---

## 9. Phased Implementation Roadmap

### Phase 1: Foundation (Days 1–3)
- [ ] Initialize repository with Rust (`cargo new ghosttree --bin`).
- [ ] Implement Git plumbing abstraction (`src/git/mod.rs`): `rev_parse`, `worktree_add`, `worktree_remove`, `worktree_list`, `merge_tree`.
- [ ] Setup SQLite database schema with `rusqlite` or `sqlx` (`src/db/mod.rs`).
- [ ] CLI parser setup using `clap` (`src/cli/mod.rs`).

### Phase 2: Process & PTY Supervisor (Days 4–6)
- [ ] Implement POSIX PTY spawner (`src/supervisor/pty.rs`) using `nix::pty::openpty`.
- [ ] Process group management and clean kill signals (`src/supervisor/proc.rs`).
- [ ] Log streaming engine to `.ghosttree/logs/<task-id>/<candidate-id>.log`.

### Phase 3: Daemon & IPC Engine (Days 7–9)
- [ ] Unix Domain Socket listener and JSON-RPC router (`src/daemon/ipc.rs`).
- [ ] State Machine dispatcher (`src/daemon/state.rs`).
- [ ] Candidate event broadcasting to connected CLI clients.

### Phase 4: Arbitrator & Evaluation (Days 10–12)
- [ ] Subprocess test runner with timeout enforcement.
- [ ] Git diff statistics parser (insertions, deletions, files modified).
- [ ] Multi-criteria scoring algorithm (`src/arbitrator/score.rs`).

### Phase 5: TUI Dashboard & Promotion (Days 13–15)
- [ ] Build interactive Terminal UI with `ratatui` (`src/ui/dashboard.rs`).
- [ ] Implement `gt merge <candidate-id>` with safe fast-forward and three-way rebase.
- [ ] End-to-end integration test suite simulating 3 parallel mock agents modifying a sample git repo.

---

## 10. Verification & Test Suite

### 10.1 Automated Test Matrix
1. **Unit Tests:**
   * Test git output parsing on edge-case commit messages, detached HEADs, and tag references.
   * Test SQLite transaction rollbacks upon unexpected process termination.
2. **Integration Tests (`tests/e2e_swarm.rs`):**
   * Script creates a temporary git repo in `/tmp/gt_test_repo`.
   * Invokes `gt swarm "Add feature"` with 3 mock bash scripts acting as agents (one succeeds, one fails tests, one times out).
   * Verifies that the correct candidate is scored highest and merged cleanly.
   * Asserts that `git worktree list` contains 0 leaked worktrees after `gt purge`.
