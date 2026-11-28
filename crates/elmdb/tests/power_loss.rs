//! LazyFS power-loss and torn-write crash verification for elmdb v2 (`DurabilityGate` + `synced.v2`).
//!
//! Run nightly and on PRs touching this file by `.github/workflows/lossy-filesystem.yml`:
//! 1. `a_filesystem_that_drops_what_was_not_synced_leaves_each_database_at_its_last_sync`:
//!    - Writer runs in `DurabilityMode::Grouped`, calls `flush()` (fsync `data.mdb` + `synced.v2` + dir)
//!      every `SYNC_EVERY` commits, writes 1..=4 unsynced commits, and aborts.
//!    - LazyFS clears its unsynced page cache (`lazyfs::clear-cache`) and drops kernel page caches.
//!    - Reopened DB MUST pass `check(None)` and hold exact `model_after(synced)`.
//!    - With `ELMDB_LOSSY_UNSYNCED=1` (negative control: `DurabilityMode::Unsafe` skipping `flush()`),
//!      LazyFS drops the claimed synced writes and the test MUST fail at the model check.
//! 2. `a_filesystem_that_persists_unsynced_data_without_meta_leaves_pinned_database_intact`:
//!    - Writer runs in `DurabilityMode::Grouped`, commits + `flush()`es snapshot `D` (40 x 80 KiB),
//!      deletes all rows in unsynced commit `D+1` (freeing `D`'s pages), ages the freelist in unsynced
//!      commit `D+2`, then arms LazyFS `torn-seq` (`op=write`, `persist=1,2` / `1,2,3`) right before
//!      unsynced commit `D+3` (~3.2 MiB data-page writes + 1 final meta-page write).
//!    - LazyFS persists ONLY the first 2..3 data-page writes of `D+3` and crashes BEFORE `D+3`'s meta
//!      write (dropping `D+1`, `D+2`, and `D+3`'s meta pages).
//!    - Pinned (`DurabilityMode::Grouped`): `MDB_RDONLY` durable pin prevents `D+3` from reusing `D`'s
//!      pages -> reopened DB MUST be `clean`, pass `check(None)`, and hold exact snapshot `D`.
//!    - With `ELMDB_LOSSY_UNPINNED=1` (`--features failpoints`, STILL `DurabilityMode::Grouped` + `flush()`,
//!      ONLY disabling the `MDB_RDONLY` durable pin via `SKIP_GROUPED_DURABLE_PIN`): `D+3`'s persisted
//!      data-page writes smash `D`'s pages on disk -> reopen / `check(None)` / model check MUST fail!
//! 3. `a_filesystem_that_tears_a_write_leaves_each_database_at_its_last_sync`:
//!    - LazyFS `torn-op` splits a write to `data.mdb` into 3 parts (`persist=[1,3]`) and crashes.
//!    - Writer runs in `DurabilityMode::Strict` until the torn write kills it.
//!    - After remounting LazyFS, reopened DB MUST pass `check(None)` and hold `model_after(synced)`
//!      (or `model_after(synced + 1)` if the tear happened after LMDB finished committing the next txn).
//! 4. `torn_sidecar_slot_falls_back_to_intact_ping_pong_slot` (always-on unit test, no FUSE required):
//!    - Tears the older slot, newer slot, or both slots of `synced.v2` and verifies dual-slot SHA-256
//!      ping-pong recovery and fail-closed rejection when neither slot is intact.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Command, Stdio};

use elmdb::protocol::{FieldDefinition, FieldSpec, IndexDefinition, OpenReport, Schema, TypeDefinition};
use elmdb::{BuildPolicy, CanonicalValue, Database, DurabilityMode, EntityTarget, ErrorCode, OpenOptions, WireOp};

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

fn held_at(dir: &Path, ids: usize) -> Result<(Model, OpenReport), String> {
    let db = Database::open(
        dir,
        OpenOptions {
            durability: DurabilityMode::Strict,
            build_policy: BuildPolicy::EagerInSchemaChange,
            ..Default::default()
        },
    )
    .map_err(|e| format!("open: {e}"))?;
    let open_report = db.open_report().clone();
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
    Ok((model, open_report))
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
    // Snapshot `synced.v2` right after commit 3 (before `db.close()` writes an extra shutdown sync
    // at the same txnid), so the two ping-pong slots hold distinct consecutive txnids (older < newer),
    // matching a real crash right as the newer slot is being written.
    let sidecar = dir.path().join("synced.v2");
    let original = std::fs::read(&sidecar).unwrap();
    db.close().unwrap();

    // Read both 4 KiB ping-pong slots from `synced.v2` and identify the older vs newer slot by seq
    // (header layout: 8-byte magic `"ELMDBV2S"`, 4-byte LE version, 8-byte LE `seq` at 12..20, `txnid` at 20..28).
    assert_eq!(original.len(), 8192);
    let slot_meta = |slot: usize| -> (u64, u64) {
        let off = slot * 4096;
        assert_eq!(&original[off..off + 8], b"ELMDBV2S");
        let seq = u64::from_le_bytes(original[off + 12..off + 20].try_into().unwrap());
        let txnid = u64::from_le_bytes(original[off + 20..off + 28].try_into().unwrap());
        (seq, txnid)
    };
    let ((seq0, txnid0), (seq1, txnid1)) = (slot_meta(0), slot_meta(1));
    assert_ne!(seq0, seq1);
    let (older_slot, newer_slot, older_txnid, newer_txnid) =
        if seq0 < seq1 { (0, 1, txnid0, txnid1) } else { (1, 0, txnid1, txnid0) };
    assert!(older_txnid < newer_txnid);

    // 1. Tearing the OLDER slot leaves the newest slot intact -> clean recovery at observed_txnid.
    let mut tear_older = original.clone();
    let older_off = older_slot * 4096;
    for b in &mut tear_older[older_off..older_off + 64] {
        *b ^= 0xFF;
    }
    std::fs::write(&sidecar, &tear_older).unwrap();
    let (recovered, report) = held_at(dir.path(), 4).unwrap();
    assert_eq!(recovered, model);
    assert_eq!(report.status, "clean");
    assert_eq!(report.valid_slots, 1);
    assert_eq!(report.durable_txnid, Some(report.observed_txnid));

    // 2. Tearing the NEWER slot falls back to the intact older slot -> valid_slots == 1, durable_txnid < observed_txnid.
    let mut tear_newer = original.clone();
    let newer_off = newer_slot * 4096;
    for b in &mut tear_newer[newer_off..newer_off + 64] {
        *b ^= 0xFF;
    }
    std::fs::write(&sidecar, &tear_newer).unwrap();
    let (recovered, report) = held_at(dir.path(), 4).unwrap();
    assert_eq!(recovered, model);
    assert_eq!(report.status, "sameBootUnsyncedTail");
    assert_eq!(report.valid_slots, 1);
    assert!(report.durable_txnid.is_some_and(|d| d < report.observed_txnid));

    // 3. Tearing BOTH slots fails closed with ErrorCode::Corrupted.
    let mut tear_both = original;
    for b in &mut tear_both[0..64] {
        *b ^= 0xFF;
    }
    for b in &mut tear_both[4096..4160] {
        *b ^= 0xFF;
    }
    std::fs::write(&sidecar, &tear_both).unwrap();
    let err = Database::open(
        dir.path(),
        OpenOptions {
            durability: DurabilityMode::Strict,
            build_policy: BuildPolicy::EagerInSchemaChange,
            ..Default::default()
        },
    )
    .err()
    .expect("corrupting both sidecar slots must fail closed");
    assert_eq!(err.code, ErrorCode::Corrupted);
}

/// Child writer invoked by the LazyFS lossy-filesystem tests below.
#[test]
#[ignore = "run only by the LazyFS lossy-filesystem tests as a child process"]
fn child_writes_until_it_dies() {
    let (Ok(path), Ok(round)) = (std::env::var("ELMDB_LOSSY_DB"), std::env::var("ELMDB_LOSSY_ROUND")) else {
        return;
    };
    let round: usize = round.parse().unwrap();
    let unsynced = std::env::var("ELMDB_LOSSY_UNSYNCED").is_ok();
    let torn = std::env::var("ELMDB_LOSSY_TORN").is_ok();

    let mode = if torn {
        DurabilityMode::Strict
    } else if unsynced {
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
    if !unsynced && !torn {
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
            if !unsynced {
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
    let unsynced = std::env::var("ELMDB_LOSSY_UNSYNCED").is_ok();

    for round in 0..4usize {
        let path = dir.join(format!("round-{round}"));
        let mut cmd = Command::new(std::env::current_exe().unwrap());
        cmd.args(["--ignored", "--exact", "child_writes_until_it_dies", "--nocapture"])
            .env("ELMDB_LOSSY_DB", &path)
            .env("ELMDB_LOSSY_ROUND", round.to_string());
        if unsynced {
            cmd.env("ELMDB_LOSSY_UNSYNCED", "1");
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

        let dropped = Command::new("sh").arg(&lose).status().unwrap();
        assert!(dropped.success(), "round {round}: lose.sh failed");

        let (actual, open_report) = held_at(&path, COMMITS).unwrap_or_else(|e| {
            panic!("round {round}, synced at {synced}: failed to reopen/check after cache drop: {e}")
        });
        assert_eq!(actual, model_after(synced), "round {round}, synced at {synced}");
        if !unsynced {
            assert_eq!(open_report.status, "clean", "round {round}: unexpected open_report {open_report:?}");
            assert_eq!(open_report.durable_txnid, Some(open_report.observed_txnid));
            assert!(open_report.valid_slots >= 1);
        }
    }
}

const TORN_SEQ_ITEMS: usize = 40;

fn torn_seq_payload(i: usize, tag: &str) -> String {
    // 80 KiB incompressible deterministic payload (40 items ≈ 3.2 MiB) so a single bulk commit
    // always produces multiple FUSE data-page writes before LMDB's final 4 KiB meta-page write.
    let mut state = 0x517c_c1b7_u32 ^ (i as u32).wrapping_mul(2_246_822_519) ^ (tag.len() as u32);
    let body: String = (0..80 * 1024)
        .map(|_| {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            char::from(b'!' + (state >> 24) as u8 % 90)
        })
        .collect();
    format!("{tag} {body}")
}

fn torn_seq_expected_model() -> Model {
    (0..TORN_SEQ_ITEMS).map(|i| (format!("Task-{i:03}"), torn_seq_payload(i, "durable"))).collect()
}

/// Child writer for LazyFS `torn-seq` freelist-reuse verification.
///
/// Stays in `DurabilityMode::Grouped` and calls `db.flush()` in BOTH pinned and unpinned modes;
/// when `ELMDB_LOSSY_UNPINNED=1`, ONLY the `MDB_RDONLY` durable pin is disabled via the
/// `SKIP_GROUPED_DURABLE_PIN` failpoint so the negative control tests the exact P0 invariant.
#[test]
#[ignore = "run only by the LazyFS torn-seq test as a child process"]
fn child_writes_torn_seq() {
    let Ok(path) = std::env::var("ELMDB_LOSSY_DB") else {
        return;
    };
    let unpinned = std::env::var("ELMDB_LOSSY_UNPINNED").is_ok();
    #[cfg(feature = "failpoints")]
    let _unpin_guard = if unpinned {
        Some(elmdb_storage::failpoints::fail_while_at(
            elmdb_storage::failpoints::points::SKIP_GROUPED_DURABLE_PIN,
            Path::new(&path),
        ))
    } else {
        None
    };
    #[cfg(not(feature = "failpoints"))]
    if unpinned {
        panic!("ELMDB_LOSSY_UNPINNED=1 requires `--features failpoints`");
    }

    let db = Database::open(
        &path,
        OpenOptions {
            durability: DurabilityMode::Grouped,
            build_policy: BuildPolicy::EagerInSchemaChange,
            map_size: Some(32 * 1024 * 1024),
            ..Default::default()
        },
    )
    .unwrap();
    db.set_schema(&schema()).unwrap();

    // Snapshot D: write 40 x 80 KiB (~3.2 MiB) and flush() so D is synced to disk and pinned.
    let create_d: Vec<WireOp> = (0..TORN_SEQ_ITEMS)
        .map(|i| WireOp::Create {
            type_name: "Task".into(),
            external_id: Some(format!("Task-{i:03}")),
            fields: vec![("c".into(), CanonicalValue::String(torn_seq_payload(i, "durable")))],
        })
        .collect();
    db.execute_batch(&create_d).unwrap();
    db.flush().unwrap();
    println!("synced {TORN_SEQ_ITEMS}");
    let _ = std::io::stdout().flush();

    // Unsynced D+1: destroy all 40 tasks so LMDB places D's ~3.2 MiB of pages onto the freelist.
    let destroy_d: Vec<WireOp> = (0..TORN_SEQ_ITEMS)
        .map(|i| WireOp::Destroy { target: EntityTarget::ExternalId(format!("Task-{i:03}")) })
        .collect();
    db.execute_batch(&destroy_d).unwrap();

    // Unsynced D+2: tiny commit aging D+1's freelist entry to 2 txns old (immediately reusable on D+3 if unpinned).
    db.execute_batch(&[WireOp::Create {
        type_name: "Task".into(),
        external_id: Some("Marker-0".into()),
        fields: vec![("c".into(), CanonicalValue::String("age-freelist".into()))],
    }])
    .unwrap();

    // Handshake with parent so LazyFS arms `torn-seq` strictly before D+3's data-page writes.
    println!("ready-for-reuse");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    let _ = std::io::stdin().read_line(&mut line);

    // Unsynced D+3: write 40 x 80 KiB (~3.2 MiB) of brand-new data pages. LazyFS persists only
    // the first 2..3 data-page writes and crashes BEFORE D+3's final meta-page write!
    let overwrite_ops: Vec<WireOp> = (0..TORN_SEQ_ITEMS)
        .map(|i| WireOp::Create {
            type_name: "Task".into(),
            external_id: Some(format!("Reuse-{i:03}")),
            fields: vec![("c".into(), CanonicalValue::String(torn_seq_payload(i, "overwritten")))],
        })
        .collect();
    let _ = db.execute_batch(&overwrite_ops);
    println!("wrote torn-seq reuse batch");
    let _ = std::io::stdout().flush();
}

/// Run over LazyFS (`.github/workflows/lossy-filesystem.yml`): persists unsynced data-page writes
/// of commit `D+3` while crashing LazyFS before any unsynced meta-page write (`D+1`, `D+2`, `D+3`)
/// hits backing storage.
///
/// - Pinned (`DurabilityMode::Grouped`): `MDB_RDONLY` durable pin keeps `D`'s pages off LMDB's
///   freelist, so reopened DB passes `check(None)` and holds exact snapshot `D`.
/// - Unpinned (`ELMDB_LOSSY_UNPINNED=1`): `SKIP_GROUPED_DURABLE_PIN` disables ONLY the durable pin
///   (staying in `DurabilityMode::Grouped` with `db.flush()`), so `D+3` overwrites `D`'s pages and
///   reopen / `check(None)` / model verification MUST fail.
#[test]
#[ignore = "run by the lossy-filesystem job, which mounts LazyFS and injects torn-seq crashes"]
fn a_filesystem_that_persists_unsynced_data_without_meta_leaves_pinned_database_intact() {
    let dir = std::path::PathBuf::from(std::env::var("ELMDB_SEQ_DIR").expect("ELMDB_SEQ_DIR"));
    let arm_seq = std::env::var("ELMDB_SEQ_ARM").expect("ELMDB_SEQ_ARM");
    let heal = std::env::var("ELMDB_TORN_HEAL").expect("ELMDB_TORN_HEAL");
    let unpinned = std::env::var("ELMDB_LOSSY_UNPINNED").is_ok();
    let expected = torn_seq_expected_model();

    for (round, persist) in ["1,2", "1,2,3"].into_iter().enumerate() {
        let path = dir.join(format!("round-{round}"));
        let mut cmd = Command::new(std::env::current_exe().unwrap());
        cmd.args(["--ignored", "--exact", "child_writes_torn_seq", "--nocapture"])
            .env("ELMDB_LOSSY_DB", &path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if unpinned {
            cmd.env("ELMDB_LOSSY_UNPINNED", "1");
        }
        let mut child = cmd.spawn().unwrap();
        let mut child_stdin = child.stdin.take().unwrap();
        let child_stdout = child.stdout.take().unwrap();
        let mut reader = BufReader::new(child_stdout);

        let mut synced = 0usize;
        let mut ready = false;
        let mut line = String::new();
        while reader.read_line(&mut line).unwrap() > 0 {
            let trimmed = line.trim();
            if let Some(rest) = trimmed.strip_prefix("synced ") {
                if let Ok(n) = rest.trim().parse::<usize>() {
                    synced = n;
                }
            } else if trimmed == "ready-for-reuse" {
                ready = true;
                break;
            }
            line.clear();
        }
        assert!(
            ready && synced == TORN_SEQ_ITEMS,
            "round {round}: child did not reach ready-for-reuse (synced={synced})"
        );

        // Arm LazyFS `torn-seq` on `data.mdb` strictly before D+3 writes begin.
        let armed =
            Command::new("sh").args([&arm_seq, path.join("data.mdb").to_str().unwrap(), persist]).status().unwrap();
        assert!(armed.success(), "round {round}: arm-seq.sh failed");

        let _ = writeln!(child_stdin, "go");
        let _ = child_stdin.flush();
        drop(child_stdin);

        let mut tail = String::new();
        let _ = std::io::Read::read_to_string(&mut reader, &mut tail);
        let _ = child.wait();
        assert!(
            !tail.contains("wrote torn-seq reuse batch"),
            "round {round}: torn-seq (persist={persist}) did not crash LazyFS before D+3 completed: {tail}"
        );

        let healed = Command::new("sh").arg(&heal).status().unwrap();
        assert!(healed.success(), "round {round}: heal.sh failed");

        let (actual, open_report) = held_at(&path, TORN_SEQ_ITEMS).unwrap_or_else(|e| {
            panic!("round {round}, synced at {synced}: failed to reopen/check after torn-seq persist={persist}: {e}")
        });
        assert_eq!(actual, expected, "round {round}, synced at {synced}: model mismatch after torn-seq");
        if !unpinned {
            assert_eq!(open_report.status, "clean", "round {round}: unexpected open_report {open_report:?}");
            assert_eq!(open_report.durable_txnid, Some(open_report.observed_txnid));
            assert!(open_report.valid_slots >= 1);
        }
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

        let (actual, open_report) = held_at(&path, COMMITS).unwrap_or_else(|e| {
            panic!("round {round}, synced at {synced}: failed to reopen/check after torn write {write}: {e}")
        });
        assert!(
            synced == 0 || open_report.valid_slots >= 1,
            "round {round}: sidecar missing after torn write at synced={synced}: {open_report:?}"
        );
        let expected_synced = model_after(synced);
        let expected_next = model_after((synced + 1).min(COMMITS));
        assert!(
            actual == expected_synced || actual == expected_next,
            "round {round}, synced at {synced}: reopened state matches neither commit {synced} nor {}",
            (synced + 1).min(COMMITS)
        );
    }
}
