//! Power loss, replayed on the data file (`docs/architecture/testing.md`, Power loss). A machine that loses power
//! keeps what was synced and any part of what was written since: each page written is on disk or isn't, whatever
//! order they were written in. The harness copies the data file after each commit, takes the pages that differ from
//! one copy to the next as what that commit wrote, and makes the files a power loss could leave:
//!
//! - **Each commit synced** (`sync_each_commit`): LMDB writes a commit's pages, syncs, and then writes the meta page
//!   that names them. So a loss leaves the commit before with any of the next one's pages and not its meta page, or
//!   the next one whole.
//! - **Commits synced in groups**: nothing is synced between two of group commit's syncs, so a loss leaves the last
//!   synced file with any of the pages written since, a meta page as soon as the pages it names.
//!
//! A file survives when it opens, `check()` finds nothing, and it holds what the commits acknowledged by then wrote,
//! or one more. This replays LMDB's order of writes as its source gives it, on a file: it is no test of a disk, a
//! filesystem or a controller that acknowledges a sync it hasn't done.

use std::collections::BTreeMap;
use std::path::Path;

use serde_json::json;

use super::{row, schema, write};
use crate::{Database, OpenOptions};

/// What the tasks hold: each id's `c`
type Model = BTreeMap<String, String>;

/// The `n`th commit of the workload, and the model after it: a task made, the one before it changed, and now and
/// then one destroyed, so that pages are freed and written over
fn commit(db: &Database, model: &mut Model, n: usize) {
    let id = |i: usize| format!("Task-{i:03}");
    // Text that doesn't compress, wide enough that the tasks fill many pages and a commit writes several
    let wide = |what: &str| {
        let mut state = 0x9e37_79b9_u32 ^ (n as u32).wrapping_mul(2_654_435_761) ^ what.len() as u32;
        let text: String = (0..5000 + 370 * (n % 9))
            .map(|_| {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                char::from(b'!' + (state >> 24) as u8 % 90)
            })
            .collect();
        format!("{what} {text}")
    };
    let mut ops = vec![json!({ "op": "create", "type": "Task", "id": id(n), "fields": { "c": wide("made") } })];
    model.insert(id(n), wide("made"));
    if n > 0 && model.contains_key(&id(n - 1)) {
        ops.push(json!({ "op": "set", "id": id(n - 1), "fields": { "c": wide("changed") } }));
        model.insert(id(n - 1), wide("changed"));
    }
    if n % 5 == 4 {
        ops.push(json!({ "op": "destroy", "id": id(n - 3) }));
        model.remove(&id(n - 3));
    }
    write(db, ops).unwrap();
}

/// The data file of the database at `dir`, as the operating system holds it
fn file(dir: &Path) -> Vec<u8> {
    std::fs::read(dir.join("data.mdb")).unwrap()
}

/// The pages of `to` that differ from `from`'s, a page `from` doesn't reach being one of zeros there
fn written(from: &[u8], to: &[u8], page: usize) -> Vec<usize> {
    let of = |file: &[u8], at: usize| {
        let mut held = vec![0u8; page];
        if let Some(part) = file.get(at * page..) {
            let part = &part[..part.len().min(page)];
            held[..part.len()].copy_from_slice(part);
        }
        held
    };
    (0..to.len().div_ceil(page)).filter(|&at| of(from, at) != of(to, at)).collect()
}

/// `from` with these pages of `to` written over it: what is on disk when those writes landed and no other did
fn with(from: &[u8], to: &[u8], pages: &[usize], page: usize) -> Vec<u8> {
    let mut image = from.to_vec();
    for &at in pages {
        let (start, end) = (at * page, ((at + 1) * page).min(to.len()));
        if image.len() < end {
            image.resize(end, 0);
        }
        image[start..end].copy_from_slice(&to[start..end]);
    }
    image
}

/// What the database whose data file is `image` holds, when it opens and `check()` finds nothing: said otherwise
fn held(image: &[u8], ids: usize) -> Result<Model, String> {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("data.mdb"), image).unwrap();
    let db =
        Database::open(dir.path(), OpenOptions { syncs: false, ..Default::default() }).map_err(|e| e.to_string())?;
    let report = db.check().map_err(|e| e.to_string())?;
    if !report.is_clean() {
        return Err(format!("check: {report:?}"));
    }
    let mut model = Model::new();
    for i in 0..ids {
        let id = format!("Task-{i:03}");
        if let Some(found) = row(&db, &id) {
            model.insert(id, found["c"].as_str().unwrap_or_default().to_owned());
        }
    }
    db.close().map_err(|e| e.to_string())?;
    Ok(model)
}

/// A few of the ways some of `pages` are written and the rest are not: none, each alone, the first half, the
/// second, every second one, and all
fn some_of(pages: &[usize]) -> Vec<Vec<usize>> {
    let half = pages.len() / 2;
    let mut some = vec![
        Vec::new(),
        pages.to_vec(),
        pages.iter().copied().step_by(2).collect(),
        pages[..half].to_vec(),
        pages[half..].to_vec(),
    ];
    some.extend(pages.iter().map(|&at| vec![at]));
    some.sort();
    some.dedup();
    some
}

const COMMITS: usize = 40;

/// The file after each commit of the workload, the first being the database as opened, with the model after each
fn run(options: OpenOptions) -> (Vec<Vec<u8>>, Vec<Model>, usize) {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(dir.path(), OpenOptions { schema: Some(schema()), ..options }).unwrap();
    let page = db.store().unwrap().page_size();
    db.durable().unwrap().wait().unwrap();
    let mut model = Model::new();
    let (mut files, mut models) = (vec![file(dir.path())], vec![model.clone()]);
    for n in 0..COMMITS {
        commit(&db, &mut model, n);
        files.push(file(dir.path()));
        models.push(model.clone());
    }
    db.close().unwrap();
    (files, models, page)
}

#[test]
fn a_commit_synced_as_it_is_made_survives_a_power_loss_at_any_write_after_it() {
    let (files, models, page) = run(OpenOptions { sync_each_commit: true, ..Default::default() });
    let mut tried = 0;
    for n in 1..files.len() {
        let (before, after) = (&files[n - 1], &files[n]);
        let pages = written(before, after, page);
        // The two meta pages are the file's first two, and a commit writes one of them, last
        let (meta, data): (Vec<usize>, Vec<usize>) = pages.iter().partition(|&&at| at < 2);
        assert_eq!(meta.len(), 1, "commit {n} writes one meta page: {pages:?}");
        assert!(!data.is_empty(), "commit {n} writes pages of its own");
        // Lost before the meta page was written, with any of the commit's pages on disk: the commit before, whole
        for some in some_of(&data) {
            let image = with(before, after, &some, page);
            assert_eq!(
                held(&image, COMMITS).as_ref(),
                Ok(&models[n - 1]),
                "commit {n}, with pages {some:?} of {data:?}"
            );
            tried += 1;
        }
        // And once the meta page is written the commit is whole
        assert_eq!(held(after, COMMITS).as_ref(), Ok(&models[n]), "commit {n}");
    }
    assert!(tried > 6 * COMMITS, "{tried} files tried");
}

/// What a child that reads a file a power loss left says of it: what it holds, or why it refused it
enum Read {
    Holds(Model),
    Refused(String),
    /// The reader was killed by a signal before it said anything: LMDB mapped a file whose meta page names pages past its
    /// end, and touched one (SIGBUS). elmdb doesn't check a file's length before mapping it, so the open itself crashes
    Crashed(String),
}

/// `held`, asked of a child of this test binary, and a file whose pages are of different commits may be one LMDB reads
/// past the end of: the child says, as its last line, what it holds or refuses, and anything else (a crash, a child
/// that ran no test) fails this test rather than counting as a file that didn't survive
fn held_apart(image: &[u8]) -> Read {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("image");
    std::fs::write(&path, image).unwrap();
    let child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--ignored", "--exact", "engine::tests::power_loss::child_says_what_a_file_holds", "--nocapture"])
        .env("ELMDB_POWER_LOSS_FILE", &path)
        .output()
        .unwrap();
    let said = String::from_utf8_lossy(&child.stdout);
    let verdict = said.lines().rev().find_map(|line| {
        line.strip_prefix("held ")
            .map(|model| Read::Holds(serde_json::from_str(model).unwrap()))
            .or_else(|| line.strip_prefix("refused ").map(|why| Read::Refused(why.to_owned())))
    });
    match (verdict, child.status.code()) {
        (Some(verdict), Some(0)) => verdict,
        // Killed by a signal, or by Windows' exception (an NTSTATUS code, with its top bits set), after the child started:
        // the file was read, and it crashed the reader
        (None, None) => Read::Crashed(format!("{}", child.status)),
        (None, Some(code)) if code as u32 >= 0xC000_0000 => Read::Crashed(format!("{}", child.status)),
        // The child exited without a verdict, or without success: it did not run the reader
        _ => panic!("the child reading the file did not say what it holds: {} {said}", child.status),
    }
}

/// The reader half of `held_apart`, run as a child of this test binary
#[test]
#[ignore = "run only by the next test, as its child process"]
fn child_says_what_a_file_holds() {
    let Ok(path) = std::env::var("ELMDB_POWER_LOSS_FILE") else { return };
    match held(&std::fs::read(path).unwrap(), COMMITS) {
        Ok(model) => println!("held {}", serde_json::to_string(&model).unwrap()),
        Err(refused) => println!("refused {}", refused.replace('\n', " ")),
    }
}

/// Whether a file a power loss left survives: it holds what some run of the commits, from the first acknowledged
/// before the loss on, wrote
fn survives(held: Result<Model, String>, models: &[Model], acknowledged: usize) -> bool {
    held.is_ok_and(|model| models[acknowledged..].contains(&model))
}

#[test]
fn commits_synced_in_groups_do_not_survive_a_power_loss_between_two_syncs() {
    // A long while between syncs, so that none runs during the workload: every commit is acknowledged, none synced
    let grouped = OpenOptions { sync_idle_ms: 600_000, sync_interval_ms: 600_000, ..Default::default() };
    let (files, models, page) = run(grouped);
    let (synced, last) = (&files[0], &files[COMMITS]);
    // With nothing written since the sync on disk, the file is whole and holds none of what was acknowledged:
    // where each commit is synced, the same loss leaves the last of them
    assert_eq!(held(synced, COMMITS).as_ref(), Ok(&models[0]));
    assert!(!survives(held(synced, COMMITS), &models, COMMITS), "every commit was acknowledged, and none is there");
    let (each, synced_models, _) = run(OpenOptions { sync_each_commit: true, ..Default::default() });
    assert!(survives(held(&each[COMMITS], COMMITS), &synced_models, COMMITS));
    // With every page on disk it is the last commit's
    assert!(survives(held(last, COMMITS), &models, COMMITS));
    // The pages written since the sync are of every commit, and nothing orders them on disk: with some of them
    // there and the rest not, the file is some commit's only by chance. Some such files don't open, or hold tasks no
    // commit left
    let pages = written(synced, last, page);
    let (meta, data): (Vec<usize>, Vec<usize>) = pages.iter().partition(|&&at| at < 2);
    assert!(!meta.is_empty() && data.len() > 8, "{pages:?}");
    let mut state = 0x2545_f491_u32;
    let mut lost = Vec::new();
    for _ in 0..24 {
        // The meta pages, and each other page or not by a coin a seed tosses
        let mut some = meta.clone();
        for &at in &data {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            if state >> 31 == 1 {
                some.push(at);
            }
        }
        match held_apart(&with(synced, last, &some, page)) {
            Read::Holds(model) if models.contains(&model) => {}
            Read::Holds(_) => lost.push("tasks no commit left".to_owned()),
            Read::Refused(why) => lost.push(why),
            Read::Crashed(how) => lost.push(format!("crashed the reader: {how}")),
        }
    }
    assert!(!lost.is_empty(), "every file of some of the pages was some commit's");
    // Refused as LMDB refuses a file it can't read, or holding no commit's tasks: a loss of some pages is not survived
}

#[test]
fn the_mode_asks_lmdb_for_each_commit_sync_and_the_grouped_mode_for_none() {
    // The flags the environment opened with say when LMDB syncs: the grouped mode syncs none of a commit, and
    // syncs each commit as it is made only with the flags off
    let (no_sync, meta_sync) = (heed::EnvFlags::NO_SYNC.bits(), heed::EnvFlags::NO_META_SYNC.bits());
    let flags_of = |options: OpenOptions| {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path(), OpenOptions { schema: Some(schema()), ..options }).unwrap();
        let flags = db.store().unwrap().env_flags();
        db.close().unwrap();
        flags
    };
    let grouped = flags_of(OpenOptions { syncs: false, ..Default::default() });
    assert_eq!(grouped & no_sync, no_sync, "the grouped mode syncs in groups");
    let each = flags_of(OpenOptions { sync_each_commit: true, ..Default::default() });
    assert_eq!(each & (no_sync | meta_sync), 0, "each commit is synced as it is made");
}
