//! The crash harness (`docs/architecture/testing.md`): a child process runs a workload over a database, acknowledging each step as it
//! commits, and is killed with `SIGKILL` at a random point; the reopened files must hold the model's state after the
//! acknowledged steps, or after one more, the step in flight having committed before its acknowledgement. The child
//! regenerates the workload from its seed, so nothing but the seed crosses.

use std::io::{BufRead, BufReader, Write as _};
use std::process::{Command, Stdio};

use proptest::strategy::ValueTree;
use proptest::test_runner::{Config, RngAlgorithm, TestRng, TestRunner};
use proptest_state_machine::ReferenceStateMachine;

use super::*;

/// The seed's workload, with no refused commit, which only a backend in this process can inject
fn seeded(seed: u64) -> Vec<Step> {
    let mut bytes = [0u8; 32];
    bytes[..8].copy_from_slice(&seed.to_le_bytes());
    let mut runner = TestRunner::new_with_rng(Config::default(), TestRng::from_seed(RngAlgorithm::ChaCha, &bytes));
    let (reference, transitions, _) =
        ModelMachine::sequential_strategy(10..60).new_tree(&mut runner).unwrap().current();
    reference
        .prelude
        .into_iter()
        .chain(transitions.into_iter().map(|t| t.step))
        .filter(|s| !matches!(s, Step::RefuseNext))
        .collect()
}

/// What differs between two states: each entity that differs and each edge one has and the other doesn't
fn differences(a: &State, b: &State) -> String {
    let mut out = Vec::new();
    for id in a.0.keys().chain(b.0.keys()).collect::<std::collections::BTreeSet<_>>() {
        if a.0.get(id) != b.0.get(id) {
            out.push(format!("{id}: {:?} / {:?}", a.0.get(id), b.0.get(id)));
        }
    }
    for e in a.1.iter().filter(|e| !b.1.contains(e)) {
        out.push(format!("edge only in the files: {e:?}"));
    }
    for e in b.1.iter().filter(|e| !a.1.contains(e)) {
        out.push(format!("edge only in the model: {e:?}"));
    }
    out.join("; ")
}

/// The writer half, run as a child of this test binary
#[test]
#[ignore = "run only by the next test, as its child process"]
fn child_runs_a_seeded_workload_acknowledging_each_step() {
    let (Ok(path), Ok(seed)) = (std::env::var("ELMDB_CRASH_DB"), std::env::var("ELMDB_CRASH_SEED")) else { return };
    // Syncs on, as the app runs: a commit survives the process's death before any sync
    let db = Database::open(
        &path,
        crate::OpenOptions {
            schema: Some(crate::schema::Schema::from_json(&super::harness_schema(true)).unwrap()),
            ..Default::default()
        },
    )
    .unwrap();
    let mut out = std::io::stdout().lock();
    for (i, step) in seeded(seed.parse().unwrap()).iter().enumerate() {
        let _ = run(&db, step);
        writeln!(out, "ack {i}").unwrap();
        out.flush().unwrap();
    }
    writeln!(out, "done").unwrap();
    out.flush().unwrap();
    // Waits to be killed, and ends with the test that started it if that ends first, however it ends: its handle
    // to this stdin closes with it
    let _ = std::io::Read::read_to_end(&mut std::io::stdin(), &mut Vec::new());
}

#[test]
fn a_killed_writer_leaves_the_state_after_its_acknowledged_steps_or_one_more() {
    for seed in 0..48u64 {
        let steps = seeded(seed);
        let dir = tempfile::tempdir().unwrap();
        // The child's own temporary directories, a rebuild's scratch among them, inside this test's: killed in the
        // middle of one, it can't remove it, and this directory's removal takes it
        let scratch = dir.path().join("scratch");
        std::fs::create_dir(&scratch).unwrap();
        let path = dir.path().join("db");
        std::fs::create_dir(&path).unwrap();
        // Somewhere past the prelude, and sometimes the whole workload
        let kill_at = 6 + (seed as usize * 7) % steps.len().max(7);
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--ignored",
                "--exact",
                "engine::tests::model::crash::child_runs_a_seeded_workload_acknowledging_each_step",
                "--nocapture",
                "--test-threads=1",
            ])
            .env("ELMDB_CRASH_DB", &path)
            .env("TMPDIR", &scratch)
            .env("TMP", &scratch)
            .env("TEMP", &scratch)
            .env("ELMDB_CRASH_SEED", seed.to_string())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut acked = 0usize;
        let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
        for line in lines.by_ref() {
            let line = line.unwrap();
            if let Some(i) = line.strip_prefix("ack ") {
                acked = i.parse::<usize>().unwrap() + 1;
                if acked >= kill_at {
                    break;
                }
            } else if line == "done" {
                break;
            }
        }
        child.kill().unwrap();
        child.wait().unwrap();
        // The child went on between the acknowledgement read and the kill: every step it acknowledged is in the pipe
        for line in lines {
            let Ok(line) = line else { break };
            if let Some(i) = line.strip_prefix("ack ") {
                acked = i.parse::<usize>().unwrap() + 1;
            }
        }
        // A child that never ran its workload leaves no files, which the model's empty state would match
        assert!(acked > 0, "seed {seed}: the child acknowledged no step");
        let db = Database::open(&path, crate::OpenOptions { syncs: false, ..Default::default() }).unwrap();
        let mut model = Model::default();
        for step in &steps[..acked] {
            let _ = model.step(step);
        }
        let after_acked = model_state(&model);
        if let Some(next) = steps.get(acked) {
            let _ = model.step(next);
        }
        let after_next = model_state(&model);
        let got = state_of(&db);
        // What the open made of the record beside the files: a death opens at the newest commit, and a restore
        // here would be the record's doing
        let restored = db.store().unwrap().restored();
        assert!(
            got == after_acked || got == after_next,
            "seed {seed}: killed after {acked} acknowledged steps, the files hold neither the state after them nor after one more\n\
             differing from the acknowledged state: {}\nthe step in flight: {:?}\nrestored to: {restored:?}",
            differences(&got, &after_acked),
            steps.get(acked)
        );
        super::declare_the_spares(&db);
        let report = db.check().unwrap();
        assert!(report.is_clean(), "seed {seed}: {report:?}");
    }
}
