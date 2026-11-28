//! Real multi-process child-process stress tests exercising cross-process LMDB + `DurabilityGate`
//! + `LiveQueryHub` contracts:
//! 1. Grouped cross-process pin-txnid window (concurrent writers + flushes across 3 processes).
//! 2. Concurrent `MAP_RESIZED` adoption across 2 child writers + 2 child readers + parent while the
//!    map doubles repeatedly from `256 KiB` -> `4+ MiB`.
//! 3. Same-process claim collision (`DatabaseLocked`), cross-process concurrent open isolation, and
//!    `SIGKILL`/hard-kill mid-write lock & stale-reader-slot recovery.
//! 4. Cross-process `LiveQueryHub` watch catch-up across creates, patches, destroys, and schema bumps.

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[cfg(not(target_os = "macos"))]
use elmdb::TransactOptions;
use elmdb::protocol::{QueryAnswerBody, QueryFilter, QuerySource, QuerySpec, QueryStep, QueryTerminal, Schema};
use elmdb::{
    BuildPolicy, CanonicalValue, Database, DurabilityMode, EntityTarget, ErrorCode, ExecBudgets, OpenOptions,
    WatchPolicy, WireOp,
};
use elmdb_storage::Vfs;

fn answer_row_count(ans: &elmdb::protocol::QueryAnswer) -> usize {
    match &ans.body {
        QueryAnswerBody::Rows { rows } => rows.len(),
        QueryAnswerBody::Ids { ids } => ids.len(),
        QueryAnswerBody::Hits { hits } => hits.len(),
        QueryAnswerBody::Count { count } => *count as usize,
        other => panic!("unexpected QueryAnswerBody: {other:?}"),
    }
}

fn immediate_watch_policy() -> WatchPolicy {
    WatchPolicy { min_gap_ms: 0, adaptive_multiplier: 0, ..WatchPolicy::default() }
}

fn stress_schema(with_priority: bool) -> Schema {
    let mut note_fields = serde_json::json!({
        "status": { "type": "string", "optional": true },
        "body": { "type": "string", "optional": true }
    });
    if with_priority {
        note_fields["priority"] = serde_json::json!({ "type": "int64", "optional": true });
    }
    serde_json::from_value(serde_json::json!({
        "types": {
            "Note": {
                "fields": note_fields,
                "indexes": {
                    "by_status": { "kind": "value", "fields": ["status"] }
                }
            },
            "Tag": {
                "fields": {
                    "label": { "type": "string", "optional": true }
                }
            }
        }
    }))
    .unwrap()
}

fn spawn_helper(helper: &str, dir: &Path, extra_env: &[(&str, &str)]) -> std::process::Child {
    let mut cmd = Command::new(std::env::current_exe().unwrap());
    cmd.args(["--ignored", "--exact", helper, "--nocapture"])
        .env("ELMDB_MP_DIR", dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    cmd.spawn().expect("spawn child helper")
}

fn wait_child_ok(child: std::process::Child, label: &str) {
    let out = child.wait_with_output().expect("wait child");
    assert!(
        out.status.success(),
        "{label} failed ({:?}):\nstdout:\n{}\nstderr:\n{}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

// ---------------------------------------------------------------------------
// 1. Grouped cross-process pin-txnid window stress
// ---------------------------------------------------------------------------

#[test]
#[ignore = "child helper for grouped_cross_process_pin_txnid_window_stress"]
fn child_grouped_writer_worker() {
    let dir = std::env::var("ELMDB_MP_DIR").expect("ELMDB_MP_DIR");
    let worker = std::env::var("ELMDB_MP_WORKER").expect("ELMDB_MP_WORKER");
    let count: usize = std::env::var("ELMDB_MP_COUNT").expect("ELMDB_MP_COUNT").parse().unwrap();

    let db = Database::open(
        &dir,
        OpenOptions {
            durability: DurabilityMode::Grouped,
            build_policy: BuildPolicy::EagerInSchemaChange,
            map_size: Some(8 * 1024 * 1024),
            ..Default::default()
        },
    )
    .unwrap();

    for i in 0..count {
        let id = format!("w{worker}-{i:03}");
        db.execute_batch(&[WireOp::Create {
            type_name: "Note".into(),
            external_id: Some(id),
            fields: vec![
                ("status".into(), CanonicalValue::String("open".into())),
                ("body".into(), CanonicalValue::String(format!("worker-{worker}-item-{i}"))),
            ],
        }])
        .unwrap();
        // Flush frequently so processes race on the `active_receipt -> prepare_durable_pin -> seal` window.
        if i % 3 == 0 {
            db.flush().unwrap();
        }
    }
    db.flush().unwrap();
    db.close().unwrap();
}

#[test]
fn grouped_cross_process_pin_txnid_window_stress() {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(
        dir.path(),
        OpenOptions {
            durability: DurabilityMode::Grouped,
            build_policy: BuildPolicy::EagerInSchemaChange,
            map_size: Some(8 * 1024 * 1024),
            ..Default::default()
        },
    )
    .unwrap();
    db.set_schema(&stress_schema(false)).unwrap();
    db.flush().unwrap();

    const PER_WORKER: usize = 24;
    let c1 =
        spawn_helper("child_grouped_writer_worker", dir.path(), &[("ELMDB_MP_WORKER", "1"), ("ELMDB_MP_COUNT", "24")]);
    let c2 =
        spawn_helper("child_grouped_writer_worker", dir.path(), &[("ELMDB_MP_WORKER", "2"), ("ELMDB_MP_COUNT", "24")]);

    // Parent writes & flushes concurrently with both child processes.
    for i in 0..PER_WORKER {
        db.execute_batch(&[WireOp::Create {
            type_name: "Note".into(),
            external_id: Some(format!("w0-{i:03}")),
            fields: vec![
                ("status".into(), CanonicalValue::String("open".into())),
                ("body".into(), CanonicalValue::String(format!("worker-0-item-{i}"))),
            ],
        }])
        .unwrap();
        if i % 2 == 0 {
            db.flush().unwrap();
        }
    }
    db.flush().unwrap();

    wait_child_ok(c1, "grouped child 1");
    wait_child_ok(c2, "grouped child 2");
    db.flush().unwrap();

    for w in 0..3usize {
        for i in 0..PER_WORKER {
            let id = format!("w{w}-{i:03}");
            let row = db.get_row(&id).unwrap().unwrap_or_else(|| panic!("missing row {id}"));
            assert_eq!(row.fields.get("body").and_then(|v| v.as_str()), Some(format!("worker-{w}-item-{i}").as_str()));
        }
    }
    assert!(db.check(None).unwrap().clean);
    db.close().unwrap();

    // Fresh reopen must be clean with valid sidecar slots.
    let reopened =
        Database::open(dir.path(), OpenOptions { durability: DurabilityMode::Grouped, ..Default::default() }).unwrap();
    assert_eq!(reopened.open_report().status, "clean");
    assert!(reopened.open_report().valid_slots >= 1);
    assert!(reopened.check(None).unwrap().clean);
    reopened.close().unwrap();
}

// ---------------------------------------------------------------------------
// 2. Concurrent MAP_RESIZED adoption across 2 child writers + 2 child readers + parent
// ---------------------------------------------------------------------------

#[test]
#[ignore = "child helper for concurrent_map_resized_adoption_across_readers_and_writers"]
fn child_map_resize_writer() {
    let dir = std::env::var("ELMDB_MP_DIR").expect("ELMDB_MP_DIR");
    let worker = std::env::var("ELMDB_MP_WORKER").expect("ELMDB_MP_WORKER");

    let db = Database::open(
        &dir,
        OpenOptions {
            durability: DurabilityMode::Grouped,
            build_policy: BuildPolicy::EagerInSchemaChange,
            map_size: Some(256 * 1024),
            ..Default::default()
        },
    )
    .unwrap();

    let blob = "B".repeat(32 * 1024);
    for i in 0..32usize {
        db.execute_batch(&[WireOp::Create {
            type_name: "Note".into(),
            external_id: Some(format!("grow-{worker}-{i:03}")),
            fields: vec![
                ("status".into(), CanonicalValue::String("grown".into())),
                ("body".into(), CanonicalValue::String(blob.clone())),
            ],
        }])
        .unwrap();
    }
    db.flush().unwrap();
    db.close().unwrap();
}

#[test]
#[ignore = "child helper for concurrent_map_resized_adoption_across_readers_and_writers"]
fn child_map_resize_reader() {
    let dir = std::env::var("ELMDB_MP_DIR").expect("ELMDB_MP_DIR");
    let done_file = std::env::var("ELMDB_MP_DONE").expect("ELMDB_MP_DONE");

    let db = Database::open(
        &dir,
        OpenOptions {
            durability: DurabilityMode::Grouped,
            read_only: true,
            map_size: Some(256 * 1024),
            ..Default::default()
        },
    )
    .unwrap();

    let spec = QuerySpec {
        source: QuerySource::Type { type_name: "Note".into(), include_trashed: false },
        steps: vec![
            QueryStep::Filter { expr: QueryFilter::Eq { field: "status".into(), value: serde_json::json!("grown") } },
            QueryStep::OrderSlice {
                by: "id".into(),
                field: None,
                dir: "asc".into(),
                locale: None,
                after_id: None,
                after_score: None,
                after_value: None,
                limit: Some(100),
            },
        ],
        terminal: Some(QueryTerminal::Rows { fields: None }),
        explain: false,
    };

    let mut reads = 0usize;
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        // Every read must transparently adopt any external `MDB_MAP_RESIZED` without error!
        let seed = db.get_row("seed-0").unwrap();
        assert!(seed.is_some(), "seed-0 must stay readable across map resizes");
        let _ans = db.execute_query_spec(&spec, ExecBudgets::default()).unwrap();
        reads += 1;
        if Path::new(&done_file).exists() && reads >= 10 {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(reads >= 10, "reader completed too few iterations: {reads}");
    assert!(db.stats().unwrap().map_size_bytes >= 2 * 1024 * 1024);
    db.close().unwrap();
}

#[test]
fn concurrent_map_resized_adoption_across_readers_and_writers() {
    let dir = tempfile::tempdir().unwrap();
    let done_file = dir.path().join(".writers-done");

    let db = Database::open(
        dir.path(),
        OpenOptions {
            durability: DurabilityMode::Grouped,
            build_policy: BuildPolicy::EagerInSchemaChange,
            map_size: Some(256 * 1024),
            ..Default::default()
        },
    )
    .unwrap();
    db.set_schema(&stress_schema(false)).unwrap();
    db.execute_batch(&[WireOp::Create {
        type_name: "Note".into(),
        external_id: Some("seed-0".into()),
        fields: vec![
            ("status".into(), CanonicalValue::String("seed".into())),
            ("body".into(), CanonicalValue::String("seed".into())),
        ],
    }])
    .unwrap();
    db.flush().unwrap();

    let done_str = done_file.to_str().unwrap();
    let r1 = spawn_helper("child_map_resize_reader", dir.path(), &[("ELMDB_MP_DONE", done_str)]);
    let r2 = spawn_helper("child_map_resize_reader", dir.path(), &[("ELMDB_MP_DONE", done_str)]);
    let w1 = spawn_helper("child_map_resize_writer", dir.path(), &[("ELMDB_MP_WORKER", "1")]);
    let w2 = spawn_helper("child_map_resize_writer", dir.path(), &[("ELMDB_MP_WORKER", "2")]);

    // Parent also reads & writes concurrently while the map grows from 256 KiB -> 4+ MiB.
    let blob = "P".repeat(32 * 1024);
    for i in 0..24usize {
        assert!(db.get_row("seed-0").unwrap().is_some());
        db.execute_batch(&[WireOp::Create {
            type_name: "Note".into(),
            external_id: Some(format!("grow-0-{i:03}")),
            fields: vec![
                ("status".into(), CanonicalValue::String("grown".into())),
                ("body".into(), CanonicalValue::String(blob.clone())),
            ],
        }])
        .unwrap();
    }
    db.flush().unwrap();

    wait_child_ok(w1, "resize writer 1");
    wait_child_ok(w2, "resize writer 2");
    std::fs::write(&done_file, b"done").unwrap();
    wait_child_ok(r1, "resize reader 1");
    wait_child_ok(r2, "resize reader 2");

    let stats = db.stats().unwrap();
    assert!(
        stats.map_size_bytes >= 2 * 1024 * 1024,
        "expected parent map_size >= 2 MiB after multi-process growth, got {}",
        stats.map_size_bytes
    );
    for (w, n) in [(0usize, 24usize), (1, 32), (2, 32)] {
        for i in 0..n {
            assert!(db.get_row(&format!("grow-{w}-{i:03}")).unwrap().is_some(), "missing grow-{w}-{i:03}");
        }
    }
    assert!(db.check(None).unwrap().clean);
    db.close().unwrap();
}

// ---------------------------------------------------------------------------
// 3. Process claim collision, cross-process open isolation & killed-writer lock recovery
// ---------------------------------------------------------------------------

#[test]
#[ignore = "child helper for process_claim_collision_and_killed_writer_lock_recovery"]
fn child_open_and_write_then_exit() {
    let dir = std::env::var("ELMDB_MP_DIR").expect("ELMDB_MP_DIR");
    let db = Database::open(&dir, OpenOptions::default()).unwrap();
    db.execute_batch(&[WireOp::Create {
        type_name: "Note".into(),
        external_id: Some("from-peer-proc".into()),
        fields: vec![("body".into(), CanonicalValue::String("ok".into()))],
    }])
    .unwrap();
    db.close().unwrap();
}

#[test]
#[ignore = "child helper for process_claim_collision_and_killed_writer_lock_recovery"]
fn child_killed_holding_reader_and_flock() {
    let dir = std::env::var("ELMDB_MP_DIR").expect("ELMDB_MP_DIR");
    // Open in Grouped mode and flush so this child holds a LIVE `MDB_RDONLY` durable pin slot in
    // `lock.mdb` at the moment it is killed, PLUS an exclusive OS flock on `synced.v2.lock`.
    let db =
        Database::open(&dir, OpenOptions { durability: DurabilityMode::Grouped, ..OpenOptions::default() }).unwrap();
    db.flush().unwrap();
    let vfs = elmdb_storage::OsVfs::new();
    let _flock = vfs.lock_file(&Path::new(&dir).join("synced.v2.lock")).unwrap();
    println!("holding-locks");
    let _ = std::io::stdout().flush();
    std::thread::sleep(Duration::from_secs(60));
}

#[cfg(not(target_os = "macos"))]
#[test]
#[ignore = "child helper for process_claim_collision_and_killed_writer_lock_recovery (Linux/Windows robust mutex)"]
fn child_killed_mid_write() {
    let dir = std::env::var("ELMDB_MP_DIR").expect("ELMDB_MP_DIR");
    let db = Database::open(&dir, OpenOptions::default()).unwrap();
    let _ = db.transact_scoped(TransactOptions::default(), || {
        db.execute_batch(&[WireOp::Create {
            type_name: "Note".into(),
            external_id: Some("uncommitted-killed".into()),
            fields: vec![("body".into(), CanonicalValue::String("must-roll-back".into()))],
        }])?;
        println!("holding-write-lock");
        let _ = std::io::stdout().flush();
        // Hold LMDB's inter-process robust write mutex until parent hard-kills us.
        std::thread::sleep(Duration::from_secs(60));
        Ok(())
    });
}

#[test]
fn process_claim_collision_and_killed_writer_lock_recovery() {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(dir.path(), OpenOptions::default()).unwrap();
    db.set_schema(&stress_schema(false)).unwrap();

    // 1. Same-process double-open MUST fail deterministically with DatabaseLocked (`OPEN_ENVS` guard).
    let dup =
        Database::open(dir.path(), OpenOptions::default()).err().expect("same-process double open must be rejected");
    assert_eq!(dup.code, ErrorCode::DatabaseLocked);

    // 2. A separate child process MUST be allowed to open and commit concurrently.
    let peer = spawn_helper("child_open_and_write_then_exit", dir.path(), &[]);
    wait_child_ok(peer, "peer process open+write");
    assert!(db.get_row("from-peer-proc").unwrap().is_some());

    // 3. Hard-killing a child holding `synced.v2.lock` (OS flock) + an LMDB reader slot must release
    //    the OS lock and allow immediate commits & stale-slot reclamation on all platforms.
    {
        let mut doomed = spawn_helper("child_killed_holding_reader_and_flock", dir.path(), &[]);
        let mut stdout = BufReader::new(doomed.stdout.take().unwrap());
        let mut saw_marker = false;
        let mut line = String::new();
        while stdout.read_line(&mut line).unwrap() > 0 {
            if line.trim() == "holding-locks" {
                saw_marker = true;
                break;
            }
            line.clear();
        }
        assert!(saw_marker, "child never printed holding-locks");
        doomed.kill().unwrap();
        let _ = doomed.wait();

        db.execute_batch(&[WireOp::Create {
            type_name: "Note".into(),
            external_id: Some("after-flock-kill".into()),
            fields: vec![("body".into(), CanonicalValue::String("recovered".into()))],
        }])
        .unwrap();
        assert!(db.get_row("after-flock-kill").unwrap().is_some());
    }

    // 4. On Linux (`PTHREAD_MUTEX_ROBUST`) and Windows (`WAIT_ABANDONED`), hard-killing a child while
    //    it holds LMDB's inter-process write mutex must also auto-recover cleanly without wedging
    //    or persisting uncommitted data. (macOS LMDB uses POSIX named semaphores which kernels do not
    //    auto-post on SIGKILL.)
    #[cfg(not(target_os = "macos"))]
    {
        let mut doomed = spawn_helper("child_killed_mid_write", dir.path(), &[]);
        let mut stdout = BufReader::new(doomed.stdout.take().unwrap());
        let mut saw_marker = false;
        let mut line = String::new();
        while stdout.read_line(&mut line).unwrap() > 0 {
            if line.trim() == "holding-write-lock" {
                saw_marker = true;
                break;
            }
            line.clear();
        }
        assert!(saw_marker, "child never printed holding-write-lock");

        doomed.kill().unwrap();
        let _ = doomed.wait();

        assert!(db.get_row("uncommitted-killed").unwrap().is_none(), "killed txn must never commit");
        db.execute_batch(&[WireOp::Create {
            type_name: "Note".into(),
            external_id: Some("after-kill".into()),
            fields: vec![("body".into(), CanonicalValue::String("recovered".into()))],
        }])
        .unwrap();
        assert!(db.get_row("after-kill").unwrap().is_some());
    }

    assert!(db.check(None).unwrap().clean);
    db.close().unwrap();
}

// ---------------------------------------------------------------------------
// 4. Cross-process watch catch-up across creates, patches, destroys, and schema bumps
// ---------------------------------------------------------------------------

#[test]
#[ignore = "child helper for cross_process_watch_catch_up_across_mutations_and_schema_bumps"]
fn child_cross_process_watch_mutator() {
    let dir = std::env::var("ELMDB_MP_DIR").expect("ELMDB_MP_DIR");
    let db = Database::open(&dir, OpenOptions::default()).unwrap();
    let stdin = std::io::stdin();
    let mut line = String::new();

    // Phase 1: create open Note.
    db.execute_batch(&[WireOp::Create {
        type_name: "Note".into(),
        external_id: Some("n1".into()),
        fields: vec![
            ("status".into(), CanonicalValue::String("open".into())),
            ("body".into(), CanonicalValue::String("v1".into())),
        ],
    }])
    .unwrap();
    println!("phase-1");
    let _ = std::io::stdout().flush();
    line.clear();
    let _ = stdin.read_line(&mut line);

    // Phase 2: patch Note status open -> done.
    db.execute_batch(&[WireOp::Patch {
        target: EntityTarget::ExternalId("n1".into()),
        fields: vec![("status".into(), CanonicalValue::String("done".into()))],
    }])
    .unwrap();
    println!("phase-2");
    let _ = std::io::stdout().flush();
    line.clear();
    let _ = stdin.read_line(&mut line);

    // Phase 3: destroy Note + create Tag.
    db.execute_batch(&[
        WireOp::Destroy { target: EntityTarget::ExternalId("n1".into()) },
        WireOp::Create {
            type_name: "Tag".into(),
            external_id: Some("t1".into()),
            fields: vec![("label".into(), CanonicalValue::String("urgent".into()))],
        },
    ])
    .unwrap();
    println!("phase-3");
    let _ = std::io::stdout().flush();
    line.clear();
    let _ = stdin.read_line(&mut line);

    // Phase 4: bump schema (adds `priority` field to `Note`).
    db.set_schema(&stress_schema(true)).unwrap();
    println!("phase-4");
    let _ = std::io::stdout().flush();
    db.close().unwrap();
}

#[test]
fn cross_process_watch_catch_up_across_mutations_and_schema_bumps() {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(dir.path(), OpenOptions::default()).unwrap();
    db.set_schema(&stress_schema(false)).unwrap();

    let note_spec = QuerySpec {
        source: QuerySource::Type { type_name: "Note".into(), include_trashed: false },
        steps: vec![QueryStep::Filter {
            expr: QueryFilter::Eq { field: "status".into(), value: serde_json::json!("open") },
        }],
        terminal: Some(QueryTerminal::Rows { fields: None }),
        explain: false,
    };
    let tag_spec = QuerySpec {
        source: QuerySource::Type { type_name: "Tag".into(), include_trashed: false },
        steps: vec![],
        terminal: Some(QueryTerminal::Rows { fields: None }),
        explain: false,
    };

    let (w_note, init_notes) = db.register_query_watch(&note_spec, immediate_watch_policy(), 100).unwrap();
    let (w_tag, init_tags) = db.register_query_watch(&tag_spec, immediate_watch_policy(), 100).unwrap();
    assert_eq!(answer_row_count(&init_notes), 0);
    assert_eq!(answer_row_count(&init_tags), 0);

    let mut cmd = Command::new(std::env::current_exe().unwrap());
    cmd.args(["--ignored", "--exact", "child_cross_process_watch_mutator", "--nocapture"])
        .env("ELMDB_MP_DIR", dir.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().unwrap();
    let mut child_in = child.stdin.take().unwrap();
    let mut child_out = BufReader::new(child.stdout.take().unwrap());

    let wait_phase = |out: &mut BufReader<std::process::ChildStdout>, expected: &str| {
        let mut s = String::new();
        while out.read_line(&mut s).unwrap() > 0 {
            if s.trim() == expected {
                return;
            }
            s.clear();
        }
        panic!("child exited before printing {expected}");
    };

    // Phase 1: Note created -> Note watch invalidated, Tag watch unaffected.
    wait_phase(&mut child_out, "phase-1");
    let dirty = db.catch_up_cross_process(200).unwrap();
    assert!(dirty.contains(&w_note), "Note watch must catch up on cross-process Create: {dirty:?}");
    assert!(!dirty.contains(&w_tag), "Tag watch should not be dirtied by Note Create: {dirty:?}");
    assert!(db.begin_watch_run(w_note));
    let (ans, changed) = db.rerun_query_watch(w_note, &note_spec, 210, 1).unwrap();
    assert!(changed && answer_row_count(&ans) == 1);

    writeln!(child_in, "next").unwrap();
    child_in.flush().unwrap();

    // Phase 2: Note patched to done -> Note watch invalidated and returns 0 open notes.
    wait_phase(&mut child_out, "phase-2");
    let dirty = db.catch_up_cross_process(300).unwrap();
    assert!(dirty.contains(&w_note), "Note watch must catch up on cross-process Patch: {dirty:?}");
    assert!(db.begin_watch_run(w_note));
    let (ans, changed) = db.rerun_query_watch(w_note, &note_spec, 310, 1).unwrap();
    assert!(changed && answer_row_count(&ans) == 0);

    writeln!(child_in, "next").unwrap();
    child_in.flush().unwrap();

    // Phase 3: Note destroyed + Tag created -> Tag watch fires and observes 1 tag.
    wait_phase(&mut child_out, "phase-3");
    let dirty = db.catch_up_cross_process(400).unwrap();
    assert!(dirty.contains(&w_tag), "Tag watch must catch up on cross-process Tag Create: {dirty:?}");
    assert!(db.begin_watch_run(w_tag));
    let (ans, changed) = db.rerun_query_watch(w_tag, &tag_spec, 410, 1).unwrap();
    assert!(changed && answer_row_count(&ans) == 1);

    writeln!(child_in, "next").unwrap();
    child_in.flush().unwrap();

    // Phase 4: Schema bump -> cross-process catch-up invalidates all watches cleanly.
    wait_phase(&mut child_out, "phase-4");
    let dirty = db.catch_up_cross_process(500).unwrap();
    assert!(
        dirty.contains(&w_note) && dirty.contains(&w_tag),
        "schema epoch bump must invalidate all active watches across processes: {dirty:?}"
    );
    assert!(db.begin_watch_run(w_note));
    let (ans, _) = db.rerun_query_watch(w_note, &note_spec, 510, 1).unwrap();
    assert_eq!(answer_row_count(&ans), 0);

    drop(child_in);
    let status = child.wait().unwrap();
    assert!(status.success(), "watch mutator child exited with {status:?}");
    db.close().unwrap();
}
