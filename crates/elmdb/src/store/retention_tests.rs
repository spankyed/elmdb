//! Controlled cross-process snapshot retention measurements. All databases and child processes are private.
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::json;

use super::*;

const KEYS: usize = 128;
const VALUE_BYTES: usize = 8192;
const ROUNDS: u8 = 32;

fn quiet() -> StoreOptions {
    StoreOptions { syncs: false, ..Default::default() }
}

struct Reader {
    child: Child,
    output: BufReader<std::process::ChildStdout>,
}

impl Reader {
    fn start(path: &Path, mode: &str) -> Self {
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args(["--ignored", "--exact", "store::retention_tests::child_idle_reader", "--nocapture"])
            .env("ELMDB_RETENTION_DB", path)
            .env("ELMDB_RETENTION_MODE", mode)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let output = BufReader::new(child.stdout.take().unwrap());
        let mut reader = Self { child, output };
        assert!(
            reader.output.by_ref().lines().any(|line| line.unwrap().contains("ELMDB_IDLE_READER_READY")),
            "reader did not reach its barrier"
        );
        reader
    }

    fn finish(&mut self) {
        writeln!(self.child.stdin.as_mut().unwrap(), "stop").unwrap();
        let mut remaining = String::new();
        self.output.read_to_string(&mut remaining).unwrap();
        assert!(self.child.wait().unwrap().success(), "reader failed: {remaining}");
    }
}

impl Drop for Reader {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
#[ignore = "measurement child; run only through the idle-reader parent"]
fn child_idle_reader() {
    let path = std::env::var("ELMDB_RETENTION_DB").expect("measurement parent supplies database");
    let mode = std::env::var("ELMDB_RETENTION_MODE").unwrap();
    assert!(mode == "cached" || mode == "released" || mode == "expired");
    // The watchdog bounds an orphan if its parent is killed before sending stop.
    std::thread::spawn(|| {
        std::thread::sleep(Duration::from_secs(60));
        std::process::exit(3);
    });
    let store = Store::open(Path::new(&path), StoreOptions { read_only: true, ..quiet() }).unwrap();
    let snapshot = store
        .read(|txn, db| {
            assert_eq!(db.get(txn, b"block0000")?, Some(vec![0; VALUE_BYTES].as_slice()));
            Ok(txn.id())
        })
        .unwrap();
    if mode == "released" {
        store.drop_idle();
    }
    if mode == "expired" {
        wait_until_empty(&store);
    }
    println!("ELMDB_IDLE_READER_READY mode={mode} snapshot={snapshot}");
    std::io::stdout().flush().unwrap();
    let mut stop = String::new();
    std::io::stdin().read_line(&mut stop).unwrap();
    assert_eq!(stop.trim(), "stop");
    store.close().unwrap();
}

fn overwrite(store: &Store, round: u8) {
    let value = vec![round; VALUE_BYTES];
    store
        .write(
            |txn, db| {
                for key in 0..KEYS {
                    db.put(txn, format!("block{key:04}").as_bytes(), &value)?;
                }
                Ok(())
            },
            false,
        )
        .unwrap();
}

fn sample(store: &Store, phase: &str, round: u8) -> serde_json::Value {
    let info = store.env.info();
    let stats = store.read(|txn, db| Ok(db.stat(txn)?)).unwrap();
    let lmdb = match std::env::var("ELMDB_MDB_STAT") {
        Ok(executable) => {
            let output = Command::new(executable).args(["-e", "-f", "-r"]).arg(store.path()).output().unwrap();
            assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
            Some(String::from_utf8(output.stdout).unwrap())
        }
        Err(_) => None,
    };
    json!({"phase":phase,"round":round,"lastTxnId":info.last_txn_id,
        "allocatedPages":info.last_page_number+1,"pageSizeBytes":stats.page_size,
        "liveTreePages":stats.branch_pages+stats.leaf_pages+stats.overflow_pages,
        "fileBytes":store.file_size(),"mapBytes":info.map_size,"lmdbStat":lmdb})
}

#[test]
#[ignore = "bounded manual measurement; scripts/measure-idle-readers.py selects it"]
fn measure_idle_reader_page_retention() {
    for readers in [1, 4] {
        for mode in ["cached", "released"] {
            let directory = tempfile::tempdir().unwrap();
            let store = Store::open(directory.path(), quiet()).unwrap();
            overwrite(&store, 0);
            let mut children: Vec<_> = (0..readers).map(|_| Reader::start(directory.path(), mode)).collect();
            // A read-once process remains idle across several application event turns before writes begin.
            std::thread::sleep(Duration::from_millis(200));
            let mut samples = vec![sample(&store, "before", 0)];
            for round in 1..=ROUNDS {
                overwrite(&store, round);
                if round.is_multiple_of(8) {
                    samples.push(sample(&store, "readers-alive", round));
                }
            }
            for child in &mut children {
                child.finish();
            }
            // Release OS process handles as well as waiting for exit (Windows PID liveness).
            drop(children);
            for round in ROUNDS + 1..=ROUNDS + 16 {
                overwrite(&store, round);
                if round.is_multiple_of(8) {
                    samples.push(sample(&store, "after-readers-close", round));
                }
            }
            println!(
                "ELMDB_RETENTION_JSON {}",
                json!({"mode":mode,"readers":readers,
                "keys":KEYS,"valueBytes":VALUE_BYTES,"syncs":false,"idleBeforeWritesMs":200,"samples":samples})
            );
        }
    }
}

#[test]
#[ignore = "manual read-overhead control, not a timing gate"]
fn measure_idle_reader_read_overhead() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path(), quiet()).unwrap();
    overwrite(&store, 0);
    let iterations = 100_000;
    for mode in ["cached", "released", "released", "cached"] {
        store.drop_idle();
        let start = Instant::now();
        for _ in 0..iterations {
            store
                .read(|txn, db| {
                    std::hint::black_box(db.get(txn, b"block0000")?);
                    Ok(())
                })
                .unwrap();
            if mode == "released" {
                // This control includes one additional mutex acquisition: report it, do not call it a
                // precise prediction of a production implementation that omits caching altogether.
                store.drop_idle();
            }
        }
        println!(
            "ELMDB_READ_OVERHEAD_JSON {}",
            json!({"mode":mode,"iterations":iterations,
            "elapsedNs":start.elapsed().as_nanos(),"controlIncludesExtraCacheLock":mode=="released"})
        );
    }
}

// Observing the private cache does not acquire a database snapshot or trigger expiration.
fn wait_until_empty(store: &Store) {
    let deadline = Instant::now() + Duration::from_secs(3);
    while store.idle.len() != 0 {
        assert!(Instant::now() < deadline, "completed snapshots were not released without another read");
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn idle_process_releases_pages_without_another_database_call() {
    let directory = tempfile::tempdir().unwrap();
    let writer = Store::open(directory.path(), quiet()).unwrap();
    overwrite(&writer, 0);
    let mut reader = Reader::start(directory.path(), "expired");
    for round in 1..=8 {
        overwrite(&writer, round);
    }
    let plateau = writer.env.info().last_page_number;
    for round in 9..=ROUNDS {
        overwrite(&writer, round);
    }
    assert_eq!(
        writer.env.info().last_page_number,
        plateau,
        "fixed-size overwrites must reuse pages while the read-once process is still alive"
    );
    reader.finish();
    writer.close().unwrap();
}

#[test]
fn active_snapshot_outlives_expiry_and_cannot_return_after_commit() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path(), quiet()).unwrap();
    overwrite(&store, 0);
    let (started, wait_started) = std::sync::mpsc::channel();
    let (resume, wait_resume) = std::sync::mpsc::channel();
    std::thread::scope(|scope| {
        let store = &store;
        scope.spawn(move || {
            store
                .read(|txn, db| {
                    let id = txn.id();
                    started.send(()).unwrap();
                    wait_resume.recv_timeout(Duration::from_secs(3)).unwrap();
                    assert_eq!(txn.id(), id);
                    assert_eq!(db.get(txn, b"block0000")?, Some(vec![0; VALUE_BYTES].as_slice()));
                    Ok(())
                })
                .unwrap();
        });
        wait_started.recv_timeout(Duration::from_secs(3)).unwrap();
        // A separate completed read expires while the first snapshot remains actively borrowed.
        store.read(|_, _| Ok(())).unwrap();
        wait_until_empty(store);
        overwrite(store, 1);
        resume.send(()).unwrap();
    });
    assert_eq!(store.idle.len(), 0, "a pre-commit active read must not repopulate the cache");
    store
        .read(|txn, db| {
            assert_eq!(db.get(txn, b"block0000")?, Some(vec![1; VALUE_BYTES].as_slice()));
            Ok(())
        })
        .unwrap();
    store.close().unwrap();
}

#[test]
fn closing_joins_idle_expiry_before_reopening() {
    let directory = tempfile::tempdir().unwrap();
    for _ in 0..8 {
        let store = Store::open(directory.path(), quiet()).unwrap();
        store.read(|_, _| Ok(())).unwrap();
        store.close().unwrap();
    }
}

#[test]
fn unit_keeps_its_uncommitted_view_across_idle_expiry() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path(), quiet()).unwrap();
    overwrite(&store, 0);
    let (mut unit, gate) = store.begin().unwrap();
    store.db.put(&mut unit, b"block0000", b"uncommitted").unwrap();
    store
        .read(|txn, db| {
            assert_eq!(db.get(txn, b"block0000")?, Some(vec![0; VALUE_BYTES].as_slice()));
            Ok(())
        })
        .unwrap();
    wait_until_empty(&store);
    assert_eq!(store.db.get(&unit, b"block0000").unwrap(), Some(b"uncommitted".as_slice()));
    unit.abort();
    drop(gate);
    store.end_unit();
    store
        .read(|txn, db| {
            assert_eq!(db.get(txn, b"block0000")?, Some(vec![0; VALUE_BYTES].as_slice()));
            Ok(())
        })
        .unwrap();
    store.close().unwrap();
}

#[test]
fn hot_reads_do_not_extend_other_cached_snapshots() {
    fn nested(store: &Store, depth: usize) {
        if depth != 0 {
            store
                .read(|_, _| {
                    nested(store, depth - 1);
                    Ok(())
                })
                .unwrap();
        }
    }
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path(), quiet()).unwrap();
    nested(&store, MAX_IDLE);
    assert_eq!(store.idle.len(), MAX_IDLE);
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        store.read(|_, _| Ok(())).unwrap();
        if store.idle.len() <= 1 {
            break;
        }
        assert!(Instant::now() < deadline, "hot reads extended other idle entries indefinitely");
        std::thread::sleep(Duration::from_millis(5));
    }
    wait_until_empty(&store);
    store.close().unwrap();
}
