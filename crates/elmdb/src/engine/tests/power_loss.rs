//! Power loss, replayed on the data file (`docs/architecture/testing.md`, Power loss). A machine that loses power
//! keeps what was synced and any part of what was written since: each page written is on disk or isn't, whatever
//! order they were written in. The harness copies the data file after each commit, takes the pages that differ from
//! one copy to the next as what that commit wrote, and makes the files a power loss could leave:
//!
//! - **Each commit synced** (`sync_each_commit`): LMDB writes a commit's pages, syncs, and then writes the meta page
//!   that names them. So a loss leaves the commit before with any of the next one's pages and not its meta page, or
//!   the next one whole.
//! - **Commits synced in groups**: nothing is synced between two of group commit's syncs, so a loss leaves the last
//!   synced file with any of the pages written since, a meta page as soon as the pages it names, and the record
//!   beside it that the last sync wrote (`docs/roadmap/synced-commit.md`). The open after the loss restores the
//!   record's commit, whose pages the pin kept from every commit after it.
//!
//! A file survives when it opens, `check()` finds nothing, and it holds what the commits acknowledged by then wrote,
//! or one more; in the grouped mode, what the last sync's commit held. This replays LMDB's order of writes as its source gives it, on a file: it is no test of a disk, a
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

/// `held` of the database in `dir`, with whatever record is beside its data file
fn held_at(dir: &Path, ids: usize) -> Result<Model, String> {
    let db = Database::open(dir, OpenOptions { syncs: false, ..Default::default() }).map_err(|e| e.to_string())?;
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

/// Every file a loss leaves of the commits of `files` in the mode that syncs each one, with `record` beside it as of
/// another boot: the commit before with any of the next one's pages, or the next one whole. Answers how many were tried
fn each_commit_survives(files: &[Vec<u8>], models: &[Model], record: &[u8], page: usize) -> usize {
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
            let dir = lost(&with(before, after, &some, page), record);
            assert_eq!(
                held_at(dir.path(), COMMITS).as_ref(),
                Ok(&models[n - 1]),
                "commit {n}, with pages {some:?} of {data:?}"
            );
            tried += 1;
        }
        // And once the meta page is written the commit is whole
        assert_eq!(held_at(lost(after, record).path(), COMMITS).as_ref(), Ok(&models[n]), "commit {n}");
    }
    tried
}

#[test]
fn a_commit_synced_as_it_is_made_survives_a_power_loss_at_any_write_after_it() {
    let dir = tempfile::tempdir().unwrap();
    let options = OpenOptions { sync_each_commit: true, ..Default::default() };
    let (files, models, record, page) = run_in(dir.path(), options, Model::new(), 0, COMMITS);
    let tried = each_commit_survives(&files, &models, &record, page);
    assert!(tried > 6 * COMMITS, "{tried} files tried");
}

/// The workload in `options` over the database at `dir`, its commits `from` on, `commits` of them, made over `model`:
/// the data file as opened and after each commit, the models, the record the open wrote, and the page size. No commit
/// writes the record: a mode that keeps no commit pays nothing for it after the open
fn run_in(
    dir: &Path,
    options: OpenOptions,
    mut model: Model,
    from: usize,
    commits: usize,
) -> (Vec<Vec<u8>>, Vec<Model>, Vec<u8>, usize) {
    let db = Database::open(dir, OpenOptions { schema: Some(schema()), ..options }).unwrap();
    let page = db.store().unwrap().page_size();
    let opened = record(dir);
    let (mut files, mut models) = (vec![file(dir)], vec![model.clone()]);
    for n in from..from + commits {
        commit(&db, &mut model, n);
        files.push(file(dir));
        models.push(model.clone());
    }
    assert!(opened == record(dir), "a commit wrote the record");
    db.close().unwrap();
    (files, models, opened, page)
}

/// A grouped writer's database as its process died, synced at its `SYNC_EVERY`th commit and three made since: the data
/// file and the record as the operating system held them, in a directory of their own, with the model and the next
/// commit's number
fn died_grouped() -> (tempfile::TempDir, Model, usize) {
    let dir = tempfile::tempdir().unwrap();
    let grouped = OpenOptions { sync_idle_ms: 600_000, sync_interval_ms: 600_000, ..Default::default() };
    let db = Database::open(dir.path(), OpenOptions { schema: Some(schema()), ..grouped }).unwrap();
    let mut model = Model::new();
    for n in 0..SYNC_EVERY + 3 {
        commit(&db, &mut model, n);
        if n + 1 == SYNC_EVERY {
            db.durable().unwrap().wait().unwrap();
        }
    }
    let died = tempfile::tempdir().unwrap();
    std::fs::write(died.path().join("data.mdb"), file(dir.path())).unwrap();
    std::fs::write(died.path().join(crate::store::RECORD_FILE), record(dir.path())).unwrap();
    db.close().unwrap();
    (died, model, SYNC_EVERY + 3)
}

#[test]
fn a_commit_synced_as_it_is_made_after_a_grouped_writer_died_survives_a_power_loss_at_any_write_after_it() {
    // The open of this mode synced what the dead writer left and wrote its own record: a loss later restores nothing,
    // where the grouped record would take the file back to a commit whose pages these commits wrote over
    let (dir, model, from) = died_grouped();
    let options = OpenOptions { sync_each_commit: true, ..Default::default() };
    let (files, models, record, page) = run_in(dir.path(), options, model, from, 12);
    let tried = each_commit_survives(&files, &models, &record, page);
    assert!(tried > 6 * 12, "{tried} files tried");
}

#[test]
fn a_database_written_with_no_sync_alone_or_after_a_grouped_writer_died_is_opened_as_it_is() {
    for after_grouped in [false, true] {
        let (dir, model, from) =
            if after_grouped { died_grouped() } else { (tempfile::tempdir().unwrap(), Model::new(), 0) };
        let options = OpenOptions { syncs: false, ..Default::default() };
        let (files, _, written_at_open, page) = run_in(dir.path(), options, model, from, 8);
        let (opened, last) = (&files[0], files.last().unwrap());
        let pages = written(opened, last, page);
        for some in tossed(&pages, 11) {
            let dir = lost(&with(opened, last, &some, page), &written_at_open);
            let before = record(dir.path());
            // Nothing is known of what such a file holds, which may end its reader: it is opened as it is, and never
            // taken back to the dead writer's last sync, which a restore would write the record for
            let _ = held_apart_in(dir.path());
            assert!(
                before == record(dir.path()),
                "after a grouped writer {after_grouped}: a restore was made, with pages {some:?} of {pages:?}"
            );
        }
    }
}

#[test]
fn a_backup_into_a_directory_whose_record_says_failed_opens_as_the_backup() {
    // The directory held a database whose sync failed: its record says `failed`, under which a file it doesn't bear
    // out is refused
    let target = tempfile::tempdir().unwrap();
    let old = Database::open(target.path(), OpenOptions { schema: Some(schema()), ..Default::default() }).unwrap();
    commit(&old, &mut Model::new(), 0);
    let store = old.store().unwrap();
    let failing = crate::failpoints::fail_while_at(crate::failpoints::points::SYNC, store.path());
    assert!(old.durable().unwrap().wait().is_err());
    drop((failing, store));
    let _ = old.close();
    // A backup of another database put there: it opens, as the backup
    let source = tempfile::tempdir().unwrap();
    let db = Database::open(source.path(), OpenOptions { schema: Some(schema()), ..Default::default() }).unwrap();
    let mut model = Model::new();
    for n in 0..5 {
        commit(&db, &mut model, n);
    }
    // A backup that fails before its copy is put there leaves the old file guarded by its record still
    let guarded = record(target.path());
    crate::failpoints::fail_once_here(crate::failpoints::points::BACKUP_PUBLISH);
    assert!(db.backup(target.path()).is_err());
    assert!(record(target.path()) == guarded, "a backup that put nothing there took the record away");
    db.backup(target.path()).unwrap();
    db.close().unwrap();
    assert_eq!(held_at(target.path(), COMMITS).as_ref(), Ok(&model));
}

#[test]
fn a_replacement_from_a_database_a_loss_left_takes_its_last_synced_commit() {
    let (files, records, models, page) = grouped_run(false);
    let ((synced, record), (next, _)) = (&records[1], &records[2]);
    // Every commit since the sync on disk, and the record not yet the next's: what the source holds is its record's
    // commit, restored, and not the commits written over its pages
    let lost_at = next - 1;
    let image = with(&files[*synced], &files[lost_at], &written(&files[*synced], &files[lost_at], page), page);
    let source = lost(&image, record);
    let target = tempfile::tempdir().unwrap();
    let db = Database::open(target.path(), OpenOptions { schema: Some(schema()), ..Default::default() }).unwrap();
    db.replace_with(source.path()).unwrap();
    db.close().unwrap();
    assert_eq!(held_at(target.path(), COMMITS).as_ref(), Ok(&models[*synced]));
}

/// What a child that reads a file a power loss left says of it: what it holds, or why it refused it
enum Read {
    Holds(Model),
    Refused(String),
    /// The reader was ended by the fault of a read past the end of a mapped file (SIGBUS) before it said anything: LMDB
    /// maps a file whose meta page names pages past its end, and touching one is that fault. `Store::open` refuses such
    /// a file before it reads a page, so this is a file that check let through: the caller fails on it
    Faulted(String),
    /// The reader was ended by LMDB's abort before it said anything, with the assertion it wrote to its error stream:
    /// built with its assertions on, as the tests' build is, LMDB aborts on a page that is no page of the tree it is
    /// in, which a file torn within its length can hold
    Aborted(String),
}

/// Whether a process was ended by the fault of a read past the end of a mapped file: a signal of its own on Unix, and
/// on Windows the exception of a read outside a mapped view or of a page that couldn't be brought in
fn faulted(status: &std::process::ExitStatus) -> bool {
    #[cfg(unix)]
    return std::os::unix::process::ExitStatusExt::signal(status) == Some(libc::SIGBUS);
    #[cfg(windows)]
    return matches!(status.code().map(|code| code as u32), Some(0xC000_0005 | 0xC000_0006));
}

/// Whether a process was ended by its own `abort`: a signal on Unix, and on Windows the fast-fail exception the C
/// runtime raises, or its older exit code
fn aborted(status: &std::process::ExitStatus) -> bool {
    #[cfg(unix)]
    return std::os::unix::process::ExitStatusExt::signal(status) == Some(libc::SIGABRT);
    #[cfg(windows)]
    return matches!(status.code().map(|code| code as u32), Some(0xC000_0409 | 3));
}

/// `held`, asked of a child of this test binary, since a file whose pages are of different commits may be one that
/// ends the process reading it: the child says, as its last line, what it holds or refuses. One ended by a fault,
/// or by an abort LMDB made of an assertion it wrote, is answered as that, and one that said nothing and ended any
/// other way fails here
fn held_apart(image: &[u8]) -> Read {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("data.mdb"), image).unwrap();
    held_apart_in(dir.path())
}

/// `held_apart` of the database in `dir`, with whatever record is beside its data file
fn held_apart_in(dir: &Path) -> Read {
    let child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--ignored", "--exact", "engine::tests::power_loss::child_says_what_a_file_holds", "--nocapture"])
        .env("ELMDB_POWER_LOSS_DIR", dir)
        .output()
        .unwrap();
    let said = String::from_utf8_lossy(&child.stdout);
    let verdict = said.lines().rev().find_map(|line| {
        // On one thread the harness writes the test's name and the child its line after it, on one line
        let line =
            line.strip_prefix("test ").and_then(|named| named.split_once(" ... ")).map_or(line, |(_, said)| said);
        line.strip_prefix("held ")
            .map(|model| Read::Holds(serde_json::from_str(model).unwrap()))
            .or_else(|| line.strip_prefix("refused ").map(|why| Read::Refused(why.to_owned())))
    });
    let errors = String::from_utf8_lossy(&child.stderr);
    // What LMDB writes before it aborts: the place and the assertion that failed
    let assertion = errors.lines().find(|line| line.contains("Assertion '"));
    match (verdict, child.status.code(), assertion) {
        (Some(verdict), Some(0), _) => verdict,
        (None, _, _) if faulted(&child.status) => Read::Faulted(format!("{}", child.status)),
        (None, _, Some(assertion)) if aborted(&child.status) => Read::Aborted(format!("{}: {assertion}", child.status)),
        // The child ended without a verdict and by neither: it did not run the reader, or something else ended it,
        // an abort that was none of LMDB's among them
        _ => panic!("the child reading the file did not say what it holds: {} {said} {errors}", child.status),
    }
}

/// The reader half of `held_apart`, run as a child of this test binary
#[test]
#[ignore = "run only by the next test, as its child process"]
fn child_says_what_a_file_holds() {
    let Ok(dir) = std::env::var("ELMDB_POWER_LOSS_DIR") else { return };
    match held_at(Path::new(&dir), COMMITS) {
        Ok(model) => println!("held {}", serde_json::to_string(&model).unwrap()),
        Err(refused) => println!("refused {}", refused.replace('\n', " ")),
    }
}

/// The record beside the data file, as the operating system holds it
fn record(dir: &Path) -> Vec<u8> {
    std::fs::read(dir.join(crate::store::RECORD_FILE)).unwrap()
}

/// How often the grouped workload syncs, in commits
const SYNC_EVERY: usize = 8;

/// What a run of the grouped workload leaves: the data file after each commit, the records with the file each was
/// written for, the models, and the page size
type Grouped = (Vec<Vec<u8>>, Vec<(usize, Vec<u8>)>, Vec<Model>, usize);

/// The grouped workload, synced at every `SYNC_EVERY`th commit and at no other: the data file after each commit, the
/// first being the database as opened, the record after each sync with the file it was written for, and the models.
/// `unpinned` turns the pin off, its transaction ended as soon as it is begun
fn grouped_run(unpinned: bool) -> Grouped {
    let dir = tempfile::tempdir().unwrap();
    let grouped = OpenOptions { sync_idle_ms: 600_000, sync_interval_ms: 600_000, ..Default::default() };
    let db = Database::open(dir.path(), OpenOptions { schema: Some(schema()), ..grouped }).unwrap();
    let store = db.store().unwrap();
    if unpinned {
        store.unpin();
    }
    let page = store.page_size();
    // The open's own commits are synced and recorded as it opens
    let mut model = Model::new();
    let (mut files, mut models) = (vec![file(dir.path())], vec![model.clone()]);
    let mut records = vec![(0, record(dir.path()))];
    for n in 0..COMMITS {
        commit(&db, &mut model, n);
        if (n + 1) % SYNC_EVERY == 0 {
            db.durable().unwrap().wait().unwrap();
            records.push((n + 1, record(dir.path())));
        }
        files.push(file(dir.path()));
        models.push(model.clone());
    }
    drop(store);
    db.close().unwrap();
    (files, records, models, page)
}

/// The database a loss left: the data file `image` and the record `record`, written in another boot
fn lost(image: &[u8], record: &[u8]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("data.mdb"), image).unwrap();
    std::fs::write(dir.path().join(crate::store::RECORD_FILE), crate::store::record_of_another_boot(record)).unwrap();
    dir
}

/// Some of the ways a loss leaves `pages` written and not: none, all, each half, every second one, and a few that a
/// seed tosses a coin for, each page in turn
fn tossed(pages: &[usize], seed: u32) -> Vec<Vec<usize>> {
    let half = pages.len() / 2;
    let mut some = vec![
        Vec::new(),
        pages.to_vec(),
        pages.iter().copied().step_by(2).collect(),
        pages[..half].to_vec(),
        pages[half..].to_vec(),
    ];
    let mut state = seed;
    for _ in 0..4 {
        some.push(
            pages
                .iter()
                .copied()
                .filter(|_| {
                    state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                    state >> 31 == 1
                })
                .collect(),
        );
    }
    some.sort();
    some.dedup();
    some
}

#[test]
fn commits_synced_in_groups_survive_a_power_loss_at_the_last_synced_commit() {
    let (files, records, models, page) = grouped_run(false);
    let mut tried = 0;
    for (window, (synced, record)) in records.iter().enumerate() {
        // The pages written after a sync, until the next is on disk: the next sync's data is synced before its
        // record is written, so a loss up to the next record leaves this one
        let next = records.get(window + 1).map_or(COMMITS, |(next, _)| *next);
        for until in [synced + 1, synced + 3, next].into_iter().filter(|until| *until <= next && *until > *synced) {
            let pages = written(&files[*synced], &files[until], page);
            for some in tossed(&pages, until as u32) {
                let dir = lost(&with(&files[*synced], &files[until], &some, page), record);
                assert_eq!(
                    held_at(dir.path(), COMMITS).as_ref(),
                    Ok(&models[*synced]),
                    "a loss after commit {until}, with pages {some:?} of {pages:?} written since the sync at {synced}"
                );
                tried += 1;
            }
        }
    }
    assert!(tried > 60, "{tried} files tried");
}

#[test]
fn a_restored_database_takes_a_workload_over_the_lost_commits_numbers_and_passes_check() {
    let (files, records, models, page) = grouped_run(false);
    let ((synced, record), (next, _)) = (&records[1], &records[2]);
    // Every commit up to the next sync's on disk but the record's: all of them lost, and their transaction numbers
    // the restored database's to use again
    let image = with(&files[*synced], &files[*next], &written(&files[*synced], &files[*next], page), page);
    let dir = lost(&image, record);
    let db = Database::open(dir.path(), OpenOptions { schema: Some(schema()), ..Default::default() }).unwrap();
    let mut model = models[*synced].clone();
    for n in *synced..next + 4 {
        commit(&db, &mut model, n);
    }
    db.durable().unwrap().wait().unwrap();
    db.close().unwrap();
    assert_eq!(held_at(dir.path(), COMMITS).as_ref(), Ok(&model));
}

#[test]
fn without_the_pin_a_power_loss_takes_the_record_s_commit_too() {
    let (files, records, models, page) = grouped_run(true);
    let mut lost_commits = Vec::new();
    for (window, (synced, record)) in records.iter().enumerate() {
        let Some((next, _)) = records.get(window + 1) else { continue };
        // Every page written since the sync on disk, its record not yet: the commits after the sync wrote over the
        // pages of its commit, which no pin kept
        let image = with(&files[*synced], &files[*next], &written(&files[*synced], &files[*next], page), page);
        let dir = lost(&image, record);
        match held_apart_in(dir.path()) {
            Read::Holds(model) if model == models[*synced] => {}
            Read::Holds(_) => lost_commits.push(format!("{synced}: tasks no commit left")),
            Read::Refused(why) => lost_commits.push(format!("{synced}: {why}")),
            Read::Aborted(how) => lost_commits.push(format!("{synced}: its reader aborted ({how})")),
            Read::Faulted(how) => panic!("a restored file was read past its end ({how})"),
        }
    }
    assert!(!lost_commits.is_empty(), "every restore without the pin was whole");
}

#[test]
fn a_file_cut_short_of_pages_its_trees_reach_is_refused_before_it_is_read() {
    let (files, _, _, page) = grouped_run(false);
    let last = &files[COMMITS];
    // Cut to half its pages, the last commit's file lacks pages its trees reach: refused by the open's check, where a
    // reader let through would fault on the first of them it touched
    match held_apart(&last[..last.len() / 2 / page * page]) {
        Read::Refused(why) => assert!(why.contains("past its end"), "{why}"),
        Read::Faulted(how) => panic!("the last commit's file, cut to half its pages, was read past its end ({how})"),
        Read::Holds(_) | Read::Aborted(_) => panic!("the last commit's file, cut to half its pages, was read"),
    }
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

/// Run by the nightly job over LazyFS (`.github/workflows/lossy-filesystem.yml`), a filesystem that keeps what a process
/// writes in a cache of its own until it is synced and drops it on a command: a loss made by a filesystem, and not a
/// replay of LMDB's order. In each round a writer in a child process commits the grouped workload, syncing at every
/// `SYNC_EVERY`th commit, and dies in its third sync once the data is flushed and before the record is written (even
/// rounds), or one to four commits after its second sync (odd rounds). The job's script, `ELMDB_LOSSY_LOSE`, then drops what
/// wasn't synced and the kernel's copy of the files. The database opened after, as of another boot, must hold what the
/// last sync the writer saw finish held, and pass `check()`. With `ELMDB_LOSSY_UNPINNED` the writer holds no pin, and
/// the job requires this to fail
#[test]
#[ignore = "run by the lossy-filesystem job, which mounts the filesystem"]
fn a_filesystem_that_drops_what_was_not_synced_leaves_each_database_at_its_last_sync() {
    let dir = std::path::PathBuf::from(std::env::var("ELMDB_LOSSY_DIR").expect("the job names the directory"));
    let lose = std::env::var("ELMDB_LOSSY_LOSE").expect("the job names the script that drops what wasn't synced");
    // Every open here is of another boot than the writer's, as after a restart
    crate::store::forget_the_boot();
    for round in 0..8usize {
        let path = dir.join(format!("round-{round}"));
        let child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--ignored", "--exact", "engine::tests::power_loss::child_writes_until_it_dies", "--nocapture"])
            .env("ELMDB_LOSSY_DB", &path)
            .env("ELMDB_LOSSY_ROUND", round.to_string())
            .output()
            .unwrap();
        assert!(!child.status.success(), "round {round}: the writer lived");
        let said = String::from_utf8_lossy(&child.stdout);
        let synced = said
            .lines()
            .filter_map(|line| line.split("synced ").nth(1))
            .filter_map(|count| count.trim().parse::<usize>().ok())
            .next_back()
            .unwrap_or_else(|| panic!("round {round}: no sync finished: {said}"));
        let lost = std::process::Command::new("sh").arg(&lose).status().unwrap();
        assert!(lost.success(), "round {round}: the script that drops what wasn't synced failed");
        // That the filesystem lost something: a writer that died between syncs left the file at its last sync, which
        // needs no restore, where the commits it made since, kept, would; one that died once a sync's data was
        // flushed left later commits on disk than its record, which a restore takes back
        // Without the pin the commits after the record's write over its pages, the file doesn't bear the record out,
        // and nothing is restored: the model's check below is the one that run is to fail
        let db = Database::open(&path, OpenOptions { syncs: false, ..Default::default() }).unwrap();
        let restored = db.open_report().restored.is_some();
        db.close().unwrap();
        assert!(
            std::env::var("ELMDB_LOSSY_UNPINNED").is_ok() || restored == round.is_multiple_of(2),
            "round {round}: restored {restored}, so the filesystem kept what wasn't synced or lost what was"
        );
        assert_eq!(held_at(&path, COMMITS).as_ref(), Ok(&model_after(synced)), "round {round}, synced at {synced}");
    }
}

/// What the workload's model holds after its first `commits` commits, made on a database of its own
fn model_after(commits: usize) -> Model {
    let dir = tempfile::tempdir().unwrap();
    let options = OpenOptions { schema: Some(schema()), syncs: false, ..Default::default() };
    let db = Database::open(dir.path(), options).unwrap();
    let mut model = Model::new();
    for n in 0..commits {
        commit(&db, &mut model, n);
    }
    db.close().unwrap();
    model
}

/// The writer of the test above, run as its child process: says each sync it saw finish, and dies as its round says
#[test]
#[ignore = "run only by the test above, as its child process"]
fn child_writes_until_it_dies() {
    let (Ok(path), Ok(round)) = (std::env::var("ELMDB_LOSSY_DB"), std::env::var("ELMDB_LOSSY_ROUND")) else { return };
    let round: usize = round.parse().unwrap();
    let grouped = OpenOptions { sync_idle_ms: 600_000, sync_interval_ms: 600_000, ..Default::default() };
    let db = Database::open(&path, OpenOptions { schema: Some(schema()), ..grouped }).unwrap();
    let store = db.store().unwrap();
    if std::env::var("ELMDB_LOSSY_UNPINNED").is_ok() {
        store.unpin();
    }
    // Torn by the filesystem: it dies of whichever write LazyFS tears, the filesystem crashing under it, and says so
    // if it lives to the end
    let torn = std::env::var("ELMDB_LOSSY_TORN").is_ok();
    let mut model = Model::new();
    for n in 0..COMMITS {
        commit(&db, &mut model, n);
        if (n + 1) % SYNC_EVERY == 0 {
            if (n + 1) / SYNC_EVERY == 3 && round.is_multiple_of(2) && !torn {
                // Its data flushed, the record not yet written
                crate::failpoints::run_once_at(crate::failpoints::points::SYNC_DATA, store.path(), || {
                    std::process::abort()
                });
            }
            db.durable().unwrap().wait().unwrap();
            println!("synced {}", n + 1);
        }
        // Rounds 1, 3, 5 and 7: one to four commits after the second sync, before the third
        if !round.is_multiple_of(2) && n + 1 == 2 * SYNC_EVERY + 1 + round / 2 && !torn {
            std::process::abort();
        }
    }
    println!("finished");
}

/// Run by the nightly job over LazyFS (`.github/workflows/lossy-filesystem.yml`), which tears a write: in each round
/// the job's script `ELMDB_TORN_ARM` mounts LazyFS again told to split the given write to the round's data file in
/// three, persist the first part and the last, and crash. A writer in a child process commits the grouped workload,
/// syncing at every `SYNC_EVERY`th commit, until the crash ends it; `ELMDB_TORN_HEAL` mounts LazyFS again as it
/// was. The database opened after, as of another boot, must hold what the writer's last finished sync held, and pass
/// `check()`. A writer that finished its workload saw no tear, which fails the round
#[test]
#[ignore = "run by the lossy-filesystem job, which mounts the filesystem and tears a write"]
fn a_filesystem_that_tears_a_write_leaves_each_database_at_its_last_sync() {
    let dir = std::path::PathBuf::from(std::env::var("ELMDB_TORN_DIR").expect("the job names the directory"));
    let arm = std::env::var("ELMDB_TORN_ARM").expect("the job names the script that sets the tear");
    let heal = std::env::var("ELMDB_TORN_HEAL").expect("the job names the script that mounts the filesystem again");
    crate::store::forget_the_boot();
    // The write to the data file torn: early in the workload and later, around its syncs
    for (round, write) in [6usize, 14, 27, 45].into_iter().enumerate() {
        let path = dir.join(format!("round-{round}"));
        let set = std::process::Command::new("sh")
            .args([&arm, path.join("data.mdb").to_str().unwrap(), &write.to_string()])
            .status()
            .unwrap();
        assert!(set.success(), "round {round}: the tear wasn't set");
        let child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--ignored", "--exact", "engine::tests::power_loss::child_writes_until_it_dies", "--nocapture"])
            .env("ELMDB_LOSSY_DB", &path)
            .env("ELMDB_LOSSY_ROUND", round.to_string())
            .env("ELMDB_LOSSY_TORN", "1")
            .output()
            .unwrap();
        let said = String::from_utf8_lossy(&child.stdout);
        assert!(!said.contains("finished"), "round {round}: write {write} wasn't torn, and the writer finished");
        // No sync finished: the record the open wrote, before any commit
        let synced = said
            .lines()
            .filter_map(|line| line.split("synced ").nth(1))
            .filter_map(|count| count.trim().parse::<usize>().ok())
            .next_back()
            .unwrap_or(0);
        let healed = std::process::Command::new("sh").arg(&heal).status().unwrap();
        assert!(healed.success(), "round {round}: the filesystem wasn't mounted again");
        assert_eq!(held_at(&path, COMMITS).as_ref(), Ok(&model_after(synced)), "round {round}, synced at {synced}");
    }
}
