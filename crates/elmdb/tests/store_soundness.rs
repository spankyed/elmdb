//! Regression tests for store soundness, `Unit` rollback/checkpoint/tracking safety, `replace_with` torn-file
//! protection, and huge `OpenOptions` sync durations.

use elmdb::engine::plan::Reads;
use elmdb::schema::Schema;
use elmdb::{Batch, Database, ErrorCode, OpenOptions};
use serde_json::json;

fn schema() -> Schema {
    Schema::from_json(&json!({
        "$schema": "https://elmdb.dev/schema/1.json",
        "types": {
            "Task": {
                "fields": {
                    "title": { "type": "string", "optional": true },
                    "body": { "type": "string", "optional": true }
                }
            }
        }
    }))
    .unwrap()
}

fn batch(ops: Vec<serde_json::Value>) -> Batch {
    serde_json::from_value(json!({ "ops": ops })).unwrap()
}

fn tasks_read(track: Option<u32>) -> Reads {
    serde_json::from_value(json!({
        "reads": [{
            "read": "query",
            "plan": {
                "start": { "from": "types", "types": ["Task"] },
                "terminal": { "answer": "ids" }
            }
        }],
        "track": track
    }))
    .unwrap()
}

fn uncompressible(bytes: usize, seed: u32) -> String {
    let mut state = seed;
    (0..bytes)
        .map(|_| {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            char::from(b'!' + (state >> 24) as u8 % 90)
        })
        .collect()
}

#[test]
fn unit_invalid_savepoint_depth_returns_batch_invalid_without_poisoning() {
    let dir = tempfile::tempdir().unwrap();
    let db =
        Database::open(dir.path(), OpenOptions { schema: Some(schema()), syncs: false, ..Default::default() }).unwrap();
    let mut unit = db.begin().unwrap();
    assert_eq!(unit.depth(), 0);
    assert_eq!(unit.rollback_to(0).unwrap_err().code, ErrorCode::BatchInvalid);
    assert_eq!(unit.rollback_to(1).unwrap_err().code, ErrorCode::BatchInvalid);
    assert_eq!(unit.release(0).unwrap_err().code, ErrorCode::BatchInvalid);
    assert_eq!(unit.release(1).unwrap_err().code, ErrorCode::BatchInvalid);

    let d1 = unit.savepoint().unwrap();
    assert_eq!(d1, 1);
    unit.write(&batch(vec![json!({ "op": "create", "type": "Task", "id": "Task-1" })])).unwrap();
    assert_eq!(unit.rollback_to(0).unwrap_err().code, ErrorCode::BatchInvalid);
    assert_eq!(unit.rollback_to(2).unwrap_err().code, ErrorCode::BatchInvalid);
    assert_eq!(unit.release(0).unwrap_err().code, ErrorCode::BatchInvalid);
    assert_eq!(unit.release(2).unwrap_err().code, ErrorCode::BatchInvalid);
    // Unit is not poisoned by invalid depths and still holds savepoint 1
    assert_eq!(unit.depth(), 1);
    unit.rollback_to(d1).unwrap();
    assert_eq!(unit.depth(), 0);
    let changes = unit.commit(false).unwrap();
    assert!(changes.entities.is_empty());
}

#[test]
fn unit_set_checkpoint_map_full_poisons_unit() {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(
        dir.path(),
        OpenOptions { schema: Some(schema()), syncs: false, map_size_bytes: Some(1 << 20), ..Default::default() },
    )
    .unwrap();
    // An overlong checkpoint name fails key validation before touching LMDB and must not poison the unit
    let mut unit = db.begin().unwrap();
    let too_long = unit.set_checkpoint(&"x".repeat(600), b"v").unwrap_err();
    assert_eq!(too_long.code, ErrorCode::ValueInvalid, "{too_long}");
    unit.set_checkpoint("ok", b"v").unwrap();
    unit.commit(false).unwrap();

    // An LMDB MapFull error during set_checkpoint must poison the unit so it cannot commit
    let mut unit = db.begin().unwrap();
    let err = unit.set_checkpoint("huge", &vec![0x5Au8; 4 << 20]).unwrap_err();
    assert_eq!(err.code, ErrorCode::MapFull, "{err}");
    // Subsequent operations and commit must refuse because the unit is poisoned
    let after = unit.commit(false).unwrap_err();
    assert_eq!(after.code, ErrorCode::MapFull, "{after}");
    assert!(after.message.contains("must be aborted"), "{after}");
}

#[test]
fn unit_rollback_to_lmdb_error_poisons_unit_and_keeps_levels_consistent() {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(
        dir.path(),
        OpenOptions { schema: Some(schema()), syncs: false, map_size_bytes: Some(1 << 20), ..Default::default() },
    )
    .unwrap();
    // Create a ~300 KiB entity committed before the unit (well under half the 1 MiB map so begin() does not grow)
    db.write(
        &batch(vec![json!({
            "op": "create",
            "type": "Task",
            "id": "Task-big",
            "fields": { "body": uncompressible(300 << 10, 1) }
        })]),
        false,
    )
    .unwrap();
    // Probe how many 32 KiB chunks fit in the remaining 1 MiB map before MapFull
    let mut unit = db.begin().unwrap();
    let mut n = 0u32;
    while unit
        .write(&batch(vec![json!({
            "op": "create",
            "type": "Task",
            "id": format!("Task-fill-{n}"),
            "fields": { "body": uncompressible(32 << 10, n + 10) }
        })]))
        .is_ok()
    {
        n += 1;
    }
    assert!(n > 2, "must have filled most of the map");
    unit.abort();
    drop(unit);

    // Now write `n - 1` chunks FIRST inside the savepoint and destroy `Task-big` LAST: rollback replays undo in
    // reverse order, so it must re-insert the 300 KiB `Task-big` BEFORE deleting the `Task-fill-*` chunks, hitting
    // MapFull mid-undo.
    let mut unit = db.begin().unwrap();
    let depth = unit.savepoint().unwrap();
    for i in 0..n.saturating_sub(1) {
        unit.write(&batch(vec![json!({
            "op": "create",
            "type": "Task",
            "id": format!("Task-fill-{i}"),
            "fields": { "body": uncompressible(32 << 10, i + 100) }
        })]))
        .unwrap();
    }
    unit.write(&batch(vec![json!({ "op": "destroy", "id": "Task-big" })])).unwrap();
    let err = unit.rollback_to(depth).unwrap_err();
    assert_eq!(err.code, ErrorCode::MapFull, "{err}");
    assert_eq!(unit.depth(), 1, "failed undo must leave savepoint level intact");
    let after = unit.commit(false).unwrap_err();
    assert_eq!(after.code, ErrorCode::MapFull, "{after}");
    assert!(after.message.contains("must be aborted"), "{after}");
}

#[test]
fn unit_read_files_tracking_on_commit_failed_reads_as_outside_a_unit_and_never_on_abort() {
    let dir = tempfile::tempdir().unwrap();
    let db =
        Database::open(dir.path(), OpenOptions { schema: Some(schema()), syncs: false, ..Default::default() }).unwrap();

    // 1. A failed tracked read inside a unit is filed at its commit as one outside a unit is: with what it read before
    // failing, which decides when it would answer otherwise
    {
        let bad = |track: u32| -> Reads {
            serde_json::from_value(json!({
                "reads": [{
                    "read": "query",
                    "plan": {
                        "start": { "from": "types", "types": ["NoSuchType"] },
                        "terminal": { "answer": "ids" }
                    }
                }],
                "track": track
            }))
            .unwrap()
        };
        assert_eq!(db.read(&bad(10)).unwrap_err().code, ErrorCode::UnknownType);
        let outside = db.track_stats();
        db.untrack(10);
        let mut unit = db.begin().unwrap();
        assert_eq!(unit.read(&bad(11)).unwrap_err().code, ErrorCode::UnknownType);
        unit.commit(false).unwrap();
        assert_eq!(db.track_stats(), outside, "a failed read is filed in a unit as outside one");
        db.untrack(11);
    }

    // 2. Succeeded tracked read inside an ABORTED unit must NOT leave its token registered
    {
        let mut unit = db.begin().unwrap();
        unit.write(&batch(vec![json!({ "op": "create", "type": "Task", "id": "Task-aborted" })])).unwrap();
        unit.read(&tasks_read(Some(12))).unwrap();
        assert_eq!(db.track_stats().0, 0, "unit read must not file before commit");
        unit.abort();
        assert_eq!(db.track_stats().0, 0, "aborted unit must not file tracked reads");
    }

    // 3. Succeeded tracked read inside a COMMITTED unit IS filed and notified by later writes
    {
        let mut unit = db.begin().unwrap();
        unit.write(&batch(vec![json!({ "op": "create", "type": "Task", "id": "Task-committed" })])).unwrap();
        unit.read(&tasks_read(Some(13))).unwrap();
        unit.commit(false).unwrap();
        assert_eq!(db.track_stats().0, 1);
        let (_, changes) =
            db.write(&batch(vec![json!({ "op": "create", "type": "Task", "id": "Task-later" })]), false).unwrap();
        assert_eq!(changes.touched, [13]);
    }
}

#[test]
fn replace_with_refuses_torn_source_file_as_corrupted() {
    let (source_dir, dest_dir) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    {
        let source = Database::open(
            source_dir.path(),
            OpenOptions { schema: Some(schema()), syncs: false, ..Default::default() },
        )
        .unwrap();
        for i in 0..40u32 {
            source
                .write(
                    &batch(vec![json!({
                        "op": "create",
                        "type": "Task",
                        "id": format!("Task-{i}"),
                        "fields": { "body": uncompressible(8 << 10, i + 1) }
                    })]),
                    false,
                )
                .unwrap();
        }
        source.close().unwrap();
    }
    // Truncate source/data.mdb to half its length so reachable tree pages lie past EOF
    let data = source_dir.path().join("data.mdb");
    let len = std::fs::metadata(&data).unwrap().len();
    std::fs::OpenOptions::new().write(true).open(&data).unwrap().set_len(len / 2).unwrap();

    let dest =
        Database::open(dest_dir.path(), OpenOptions { schema: Some(schema()), syncs: false, ..Default::default() })
            .unwrap();
    let err = dest.replace_with(source_dir.path()).unwrap_err();
    assert_eq!(err.code, ErrorCode::Corrupted, "{err}");
    dest.close().unwrap();
}

#[test]
fn open_with_huge_sync_durations_does_not_panic() {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(
        dir.path(),
        OpenOptions {
            schema: Some(schema()),
            syncs: true,
            sync_idle_ms: u64::MAX,
            sync_interval_ms: u64::MAX,
            ..Default::default()
        },
    )
    .unwrap();
    db.write(&batch(vec![json!({ "op": "create", "type": "Task", "id": "Task-1" })]), false).unwrap();
    db.durable().unwrap().wait().unwrap();
    db.close().unwrap();
}
