//! LazyFS power-loss and torn-write crash verification for elmdb v2 (`DurabilityGate` + `synced.v2`).
//!
//! Run nightly and on PRs touching this file by `.github/workflows/lossy-filesystem.yml`:
//! 1. `a_filesystem_that_drops_what_was_not_synced_leaves_each_database_at_its_last_sync`:
//!    - Writer runs in `DurabilityMode::Grouped`, calls `flush()` (fsync `data.mdb` + `synced.v2` + dir)
//!      every `SYNC_EVERY` commits, writes 1..=4 unsynced commits, and aborts.
//!    - LazyFS clears its unsynced page cache (`lazyfs::clear-cache`) and drops kernel page caches.
//!    - Reopened DB MUST pass `check(None)` and hold exact `model_after(synced)`.
//!    - With `ELMDB_LOSSY_UNPINNED=1` (negative control: `DurabilityMode::Unsafe` skipping `flush()`),
//!      LazyFS drops the claimed synced writes and the test MUST fail at the model check.
//! 2. `a_filesystem_that_tears_a_write_leaves_each_database_at_its_last_sync`:
//!    - LazyFS `torn-op` splits a write to `data.mdb` into 3 parts (`persist=[1,3]`) and crashes.
//!    - Writer runs in `DurabilityMode::Strict` until the torn write kills it.
//!    - After remounting LazyFS, reopened DB MUST pass `check(None)` and hold `model_after(synced)`
//!      (or `model_after(synced + 1)` if the tear happened after LMDB finished committing the next txn).
//! 3. `torn_sidecar_slot_falls_back_to_intact_ping_pong_slot` (always-on unit test, no FUSE required):
//!    - Tears slot 0 or slot 1 of `synced.v2` and verifies dual-slot CRC32 ping-pong recovery.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;

use elmdb::protocol::{FieldDefinition, FieldSpec, IndexDefinition, Schema, TypeDefinition};
use elmdb::{BuildPolicy, CanonicalValue, Database, DurabilityMode, EntityTarget, OpenOptions, WireOp};

const COMMITS: usize = 24;
const SYNC_EVERY: usize = 6;

type Model = BTreeMap<String, String>;

fn schema() -> Schema {
    let mut fields = BTreeMap::new();
    fields.insert("c".into(), FieldDefinition { kind: FieldSpec::String, optional: false, nullable: false });
    let mut indexes = BTreeMap::new();
    indexes.insert("by_c".into(), IndexDefinition::Text { fields: vec![("c".into(), 1)] });
    let mut types = BTreeMap::new();
    types.insert("Task".into(), TypeDefinition { fields, indexes, ..Default::default() });
    Schema { dollar_schema: None, types, relations: BTreeMap::new() }
}

fn wide(n: usize, what: &str) -> String {
    // Incompressible deterministic payload large enough to span multiple 4 KiB/16 KiB LMDB pages.
    let mut state = 0x9e37_79b9_u32 ^ (n as u32).wrapping_mul(2_654_435_761) ^ what.len() as u32;
    let text: String = (0..5000 + 370 * (n % 9))
        .map(|_| {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            char::from(b'!' + (state >> 24) as u8 % 90)
        })
        .collect();
    format!("{what} {text}")
}

fn commit(db: &Database, model: &mut Model, n: usize) {
    let id = |i: usize| format!("Task-{i:03}");
    let mut ops = vec![WireOp::Create {
        type_name: "Task".into(),
        external_id: Some(id(n)),
        fields: vec![("c".into(), CanonicalValue::String(wide(n, "made")))],
    }];
    model.insert(id(n), wide(n, "made"));
    if n > 0 && model.contains_key(&id(n - 1)) {
        ops.push(WireOp::Patch {
            target: EntityTarget::ExternalId(id(n - 1)),
            fields: vec![("c".into(), CanonicalValue::String(wide(n, "changed")))],
        });
        model.insert(id(n - 1), wide(n, "changed"));
    }
    if n % 5 == 4 {
        ops.push(WireOp::Destroy { target: EntityTarget::ExternalId(id(n - 3)) });
        model.remove(&id(n - 3));
    }
    db.execute_batch(&ops).unwrap();
}

fn model_after(count: usize) -> Model {
    let mut model = Model::new();
    let id = |i: usize| format!("Task-{i:03}");
    for n in 0..count {
        model.insert(id(n), wide(n, "made"));
        if n > 0 && model.contains_key(&id(n - 1)) {
            model.insert(id(n - 1), wide(n, "changed"));
        }
        if n % 5 == 4 {
            model.remove(&id(n - 3));
        }
    }
    model
}

fn held_at(dir: &Path, ids: usize) -> Result<Model, String> {
    let db = Database::open(
        dir,
        OpenOptions {
            durability: DurabilityMode::Strict,
            build_policy: BuildPolicy::EagerInSchemaChange,
            ..Default::default()
        },
    )
    .map_err(|e| format!("open: {e}"))?;
    let report = db.check(None).map_err(|e| format!("check: {e}"))?;
    if !report.clean {
        return Err(format!("check not clean: {report:?}"));
    }
    let mut model = Model::new();
    for i in 0..ids {
        let id = format!("Task-{i:03}");
        if let Some(row) = db.get_row_resolved(&id).map_err(|e| format!("get {id}: {e}"))? {
            let val = row
                .fields
                .get("c")
                .and_then(|v| v.as_str())
                .ok_or_else(|| format!("missing string field c on {id}"))?;
            model.insert(id, val.to_owned());
        }
    }
    db.close().map_err(|e| format!("close: {e}"))?;
    Ok(model)
}

#[test]
fn torn_sidecar_slot_falls_back_to_intact_ping_pong_slot() {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(
        dir.path(),
        OpenOptions {
            durability: DurabilityMode::Strict,
            build_policy: BuildPolicy::EagerInSchemaChange,
            ..Default::default()
        },
    )
    .unwrap();
    db.set_schema(&schema()).unwrap();
    let mut model = Model::new();
    for n in 0..4 {
        commit(&db, &mut model, n);
    }
    db.close().unwrap();

    // Corrupt slot 1 (second 4 KiB ping-pong slot of synced.v2) while leaving slot 0 intact,
    // or corrupt whichever slot has the lower sequence? Wait: after 5 strict writes (schema + 4 commits),
    // both slots hold valid CRC32-verified slots (slot 0 = seq 5, slot 1 = seq 4).
    // Tearing the OLDER slot (slot 1) MUST leave slot 0 active and recover cleanly at seq 5!
    let sidecar = dir.path().join("synced.v2");
    let mut bytes = std::fs::read(&sidecar).unwrap();
    assert_eq!(bytes.len(), 8192);
    // Corrupt bytes in slot 1 (offset 4096..4160) to simulate a torn write to the alternate slot.
    for b in &mut bytes[4096..4160] {
        *b ^= 0xFF;
    }
    std::fs::write(&sidecar, &bytes).unwrap();

    let recovered = held_at(dir.path(), 4).unwrap();
    assert_eq!(recovered, model);
}

/// Child writer invoked by the LazyFS lossy-filesystem tests below.
#[test]
#[ignore = "run only by the LazyFS lossy-filesystem tests as a child process"]
fn child_writes_until_it_dies() {
    let (Ok(path), Ok(round)) = (std::env::var("ELMDB_LOSSY_DB"), std::env::var("ELMDB_LOSSY_ROUND")) else {
        return;
    };
    let round: usize = round.parse().unwrap();
    let unpinned = std::env::var("ELMDB_LOSSY_UNPINNED").is_ok();
    let torn = std::env::var("ELMDB_LOSSY_TORN").is_ok();

    let mode = if torn {
        DurabilityMode::Strict
    } else if unpinned {
        DurabilityMode::Unsafe
    } else {
        DurabilityMode::Grouped
    };

    let db = Database::open(
        &path,
        OpenOptions { durability: mode, build_policy: BuildPolicy::EagerInSchemaChange, ..Default::default() },
    )
    .unwrap();
    db.set_schema(&schema()).unwrap();
    if !unpinned && !torn {
        // Persist initial schema before starting grouped workload so even synced == 0 is clean.
        db.flush().unwrap();
    }

    let mut model = Model::new();
    for n in 0..COMMITS {
        commit(&db, &mut model, n);
        if torn {
            // Strict mode fsyncs every commit before returning.
            println!("synced {}", n + 1);
            let _ = std::io::stdout().flush();
            continue;
        }
        if (n + 1) % SYNC_EVERY == 0 {
            if !unpinned {
                db.flush().unwrap();
            }
            println!("synced {}", n + 1);
            let _ = std::io::stdout().flush();
        }
        // Crash 1..=4 unsynced commits after the 2nd sync (at commit 12), before the 3rd sync (at commit 18).
        if n + 1 == 2 * SYNC_EVERY + 1 + (round % 4) {
            std::process::abort();
        }
    }
    println!("wrote every commit of the workload");
    let _ = std::io::stdout().flush();
}

/// Run over LazyFS (`.github/workflows/lossy-filesystem.yml`): drops all unsynced writes after child aborts.
#[test]
#[ignore = "run by the lossy-filesystem job, which mounts LazyFS and clears its unsynced cache"]
fn a_filesystem_that_drops_what_was_not_synced_leaves_each_database_at_its_last_sync() {
    let dir = std::path::PathBuf::from(std::env::var("ELMDB_LOSSY_DIR").expect("ELMDB_LOSSY_DIR"));
    let lose = std::env::var("ELMDB_LOSSY_LOSE").expect("ELMDB_LOSSY_LOSE");
    let unpinned = std::env::var("ELMDB_LOSSY_UNPINNED").is_ok();

    for round in 0..4usize {
        let path = dir.join(format!("round-{round}"));
        let mut cmd = std::process::Command::new(std::env::current_exe().unwrap());
        cmd.args(["--ignored", "--exact", "child_writes_until_it_dies", "--nocapture"])
            .env("ELMDB_LOSSY_DB", &path)
            .env("ELMDB_LOSSY_ROUND", round.to_string());
        if unpinned {
            cmd.env("ELMDB_LOSSY_UNPINNED", "1");
        }
        let child = cmd.output().unwrap();
        assert!(!child.status.success(), "round {round}: child should have aborted mid-workload");
        let said = String::from_utf8_lossy(&child.stdout);
        let synced = said
            .lines()
            .filter_map(|line| line.split("synced ").nth(1))
            .filter_map(|count| count.trim().parse::<usize>().ok())
            .next_back()
            .unwrap_or(0);
        assert!(synced > 0, "round {round}: writer never reached a sync: {said}");

        let dropped = std::process::Command::new("sh").arg(&lose).status().unwrap();
        assert!(dropped.success(), "round {round}: lose.sh failed");

        assert_eq!(held_at(&path, COMMITS).as_ref(), Ok(&model_after(synced)), "round {round}, synced at {synced}");
    }
}

/// Run over LazyFS (`.github/workflows/lossy-filesystem.yml`): tears a write to `data.mdb` and crashes LazyFS.
#[test]
#[ignore = "run by the lossy-filesystem job, which mounts LazyFS and injects torn-op crashes"]
fn a_filesystem_that_tears_a_write_leaves_each_database_at_its_last_sync() {
    let dir = std::path::PathBuf::from(std::env::var("ELMDB_TORN_DIR").expect("ELMDB_TORN_DIR"));
    let arm = std::env::var("ELMDB_TORN_ARM").expect("ELMDB_TORN_ARM");
    let heal = std::env::var("ELMDB_TORN_HEAL").expect("ELMDB_TORN_HEAL");

    // Tear writes at multiple points across the workload (early, mid, and late).
    for (round, write) in [6usize, 14, 27, 45].into_iter().enumerate() {
        let path = dir.join(format!("round-{round}"));
        let set = std::process::Command::new("sh")
            .args([&arm, path.join("data.mdb").to_str().unwrap(), &write.to_string()])
            .status()
            .unwrap();
        assert!(set.success(), "round {round}: arm.sh failed");

        let child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--ignored", "--exact", "child_writes_until_it_dies", "--nocapture"])
            .env("ELMDB_LOSSY_DB", &path)
            .env("ELMDB_LOSSY_ROUND", round.to_string())
            .env("ELMDB_LOSSY_TORN", "1")
            .output()
            .unwrap();
        let said = String::from_utf8_lossy(&child.stdout);
        assert!(
            !said.lines().any(|line| line.contains("wrote every commit of the workload")),
            "round {round}: write {write} was not torn and writer finished: {said}"
        );
        let synced = said
            .lines()
            .filter_map(|line| line.split("synced ").nth(1))
            .filter_map(|count| count.trim().parse::<usize>().ok())
            .next_back()
            .unwrap_or(0);

        let healed = std::process::Command::new("sh").arg(&heal).status().unwrap();
        assert!(healed.success(), "round {round}: heal.sh failed");

        let actual = held_at(&path, COMMITS).unwrap_or_else(|e| {
            panic!("round {round}, synced at {synced}: failed to reopen/check after torn write {write}: {e}")
        });
        let expected_synced = model_after(synced);
        let expected_next = model_after((synced + 1).min(COMMITS));
        assert!(
            actual == expected_synced || actual == expected_next,
            "round {round}, synced at {synced}: reopened state matches neither commit {synced} nor {}",
            (synced + 1).min(COMMITS)
        );
    }
}
