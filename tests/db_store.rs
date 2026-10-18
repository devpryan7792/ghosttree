//! Integration tests for the SQLite WAL store (SPEC §3.1, §10.1).
//!
//! File-backed scenarios complement the in-memory unit tests in src/db/mod.rs
//! (WAL pragma, schema persistence, cross-connection visibility).

use ghosttree::db::{
    CandidateStatus, DbError, EventType, NewCandidate, NewTask, Store, TaskStatus, TaskStrategy,
    TestResults,
};

fn task(id: &str) -> NewTask {
    NewTask {
        id: id.to_string(),
        title: format!("Task {id}"),
        prompt: "do the thing".into(),
        base_commit: "7b31cf2".into(),
        base_branch: "main".into(),
        strategy: TaskStrategy::Competing,
    }
}

fn candidate(task_id: &str, id: &str) -> NewCandidate {
    NewCandidate {
        id: id.to_string(),
        task_id: task_id.to_string(),
        variant_name: id.to_string(),
        worktree_path: "/tmp/wt".into(),
        branch_name: format!("ghosttree/{task_id}/{id}"),
        agent_cmd: "agent --run".into(),
    }
}

#[test]
fn schema_persists_across_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ghosttree.db");
    {
        let store = Store::open(&path).unwrap();
        store.insert_task(&task("t1")).unwrap();
        store.insert_candidate(&candidate("t1", "c1")).unwrap();
    }
    let store = Store::open(&path).unwrap();
    assert!(store.get_task("t1").unwrap().is_some());
    assert_eq!(store.list_candidates("t1").unwrap().len(), 1);
}

#[test]
fn wal_journal_mode_is_active_on_file_backed_stores() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("ghosttree.db")).unwrap();
    assert_eq!(store.journal_mode().unwrap().to_lowercase(), "wal");
    // WAL sidecar files appear after a write transaction.
    store.insert_task(&task("t1")).unwrap();
    let wal = dir.path().join("ghosttree.db-wal");
    assert!(wal.exists(), "expected SQLite to create {wal:?}");
}

#[test]
fn cross_connection_visibility_under_wal() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ghosttree.db");
    let store_a = Store::open(&path).unwrap();
    let store_b = Store::open(&path).unwrap(); // concurrent reader connection

    store_a.insert_task(&task("t1")).unwrap();
    store_a.insert_candidate(&candidate("t1", "c1")).unwrap();
    store_a
        .set_candidate_status("c1", CandidateStatus::Testing)
        .unwrap();

    // The second connection must observe committed rows immediately.
    let seen = store_b.get_candidate("c1").unwrap().unwrap();
    assert_eq!(seen.status.as_ref().unwrap(), &CandidateStatus::Testing);
}

#[test]
fn transactional_insert_visible_only_after_commit() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ghosttree.db");
    let mut store = Store::open(&path).unwrap();
    {
        let tx = store.transaction().unwrap();
        tx.execute(
            "INSERT INTO tasks (id, title, prompt, base_commit, base_branch, strategy, status, created_at, updated_at)
             VALUES ('t9', 'T', 'P', 'abc', 'main', 'competing', 'PENDING', 1, 1)",
            [],
        )
        .unwrap();
        // Not yet committed: another connection must not see it.
        let other = Store::open(&path).unwrap();
        assert!(other.get_task("t9").unwrap().is_none());
        tx.commit().unwrap();
    }
    let other = Store::open(&path).unwrap();
    assert!(other.get_task("t9").unwrap().is_some());
}

#[test]
fn cascade_delete_across_schema() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("ghosttree.db")).unwrap();
    store.insert_task(&task("t1")).unwrap();
    store.insert_candidate(&candidate("t1", "c1")).unwrap();
    store
        .append_event("c1", EventType::AgentStdout, "running")
        .unwrap();

    assert_eq!(store.delete_task("t1").unwrap(), 1);
    assert!(store.get_task("t1").unwrap().is_none());
    assert!(store.get_candidate("c1").unwrap().is_none());
    assert!(store.list_events("c1").unwrap().is_empty());
}

#[test]
fn test_results_and_diff_stat_roundtrip_on_file_db() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("ghosttree.db")).unwrap();
    store.insert_task(&task("t1")).unwrap();
    store.insert_candidate(&candidate("t1", "c1")).unwrap();

    store
        .set_candidate_test_results(
            "c1",
            &TestResults {
                passed: 8,
                failed: 0,
                duration_ms: 420,
            },
        )
        .unwrap();
    let stored = store
        .get_candidate("c1")
        .unwrap()
        .unwrap()
        .test_results()
        .unwrap()
        .unwrap();
    assert_eq!(stored.passed, 8);
    assert_eq!(stored.failed, 0);
    assert_eq!(stored.duration_ms, 420);
}

#[test]
fn unknown_event_type_read_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ghosttree.db");
    let mut store = Store::open(&path).unwrap();
    store.insert_task(&task("t1")).unwrap();
    store.insert_candidate(&candidate("t1", "c1")).unwrap();
    {
        let tx = store.transaction().unwrap();
        tx.execute(
            "INSERT INTO candidate_events (candidate_id, event_type, payload, timestamp)
             VALUES ('c1', 'WEIRD', 'p', 1)",
            [],
        )
        .unwrap();
        tx.commit().unwrap();
    }
    let events = store.list_events("c1").unwrap();
    assert!(matches!(
        events[0].event_type,
        Err(DbError::UnknownEventType(_))
    ));
}

#[test]
fn status_transitions_update_rows() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("ghosttree.db")).unwrap();
    store.insert_task(&task("t1")).unwrap();
    store.insert_candidate(&candidate("t1", "c1")).unwrap();

    assert_eq!(
        store.update_task_status("t1", TaskStatus::Running).unwrap(),
        1
    );
    assert_eq!(
        store.get_task("t1").unwrap().unwrap().status.unwrap(),
        TaskStatus::Running
    );

    store.set_task_winner("t1", "c1").unwrap();
    assert_eq!(
        store.get_task("t1").unwrap().unwrap().winner_candidate_id,
        Some("c1".to_string())
    );
}
