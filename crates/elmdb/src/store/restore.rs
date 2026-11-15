//! What an open does with the record before LMDB opens the file (`docs/roadmap/synced-commit.md`, What an open does),
//! holding the record's lock: whether the file is opened as it is or at the record's commit, the restore that takes it
//! there, and the record an open of another mode writes for its mode.
//!
//! A restore is the one write to a file LMDB owns. It is made only by an open that holds the database alone, no other
//! process having it open: another process takes its next transaction's number from LMDB's lock file and its trees
//! from the meta page, and would build on the commits being undone. Stopped at any step, the next open makes it again
//! from the same record.

use std::fs::File;
use std::io::{Seek, SeekFrom, Write};
use std::path::Path;

use super::boot::Boot;
use super::reach;
use super::record::{self, Mode, Record, RecordFile, State};
use crate::error::{Error, ErrorCode, Result};

/// What the open decided, for the open to go on with
pub(super) struct Decided {
    /// The lock that says this process has the database open, held shared for as long as it does
    pub presence: File,
    /// The record's file, for a writable open
    pub record: Option<RecordFile>,
    /// The commit of the grouped mode's record, where the file bears it out: what the open's process may rely on
    pub remembered: Option<u64>,
    /// A restore was made to this commit and no caller has said it read so
    pub restored: Option<u64>,
    /// The record named a file other than this one, which was opened as it is
    pub not_borne_out: bool,
}

/// How an open goes on, by its record
#[derive(Debug, PartialEq)]
enum Outcome {
    AsIs,
    /// A record of another boot whose commit is the file's newest: nothing was lost, and a writable open writes the
    /// record again as of this boot
    Unchanged,
    Restore,
    NotBorneOut,
}

/// Decides how the database in `dir` is opened and makes any restore, before LMDB opens it. `mode` is a writable
/// open's, none for a read-only one
pub(super) fn decide(dir: &Path, mode: Option<Mode>, boot: Boot) -> Result<Decided> {
    let decide = record::lock_file(dir, record::RECORD_LOCK)?;
    let _deciding = record::exclusive(&decide)?;
    let presence = record::lock_file(dir, record::PRESENCE)?;
    let alone = match presence.try_lock() {
        Ok(()) => true,
        Err(std::fs::TryLockError::WouldBlock) => false,
        Err(std::fs::TryLockError::Error(error)) => return Err(error.into()),
    };
    let writable = mode.is_some();
    let mut file = RecordFile::open(dir, writable)?;
    let mut record = match file.as_mut() {
        Some(file) => file.read()?,
        None => None,
    };
    // Relied on only once synced: the process that wrote it may have died before it did. A read-only open can't sync
    // it, and relies on nothing
    if let (Some(file), Some(_)) = (file.as_mut().filter(|_| writable), record.as_ref()) {
        file.sync()?;
    }
    let data = dir.join("data.mdb");
    let outcome = match &record {
        None => Outcome::AsIs,
        Some(record) if !alone => {
            if record.mode == Mode::Grouped && record.state == State::Failed {
                return Err(pending(dir, "another process has it open"));
            }
            Outcome::AsIs
        }
        Some(record) => decided(&data, record, boot)?,
    };
    let mut restored = record.as_ref().and_then(|record| record.restored);
    match outcome {
        Outcome::AsIs => {}
        Outcome::NotBorneOut => {
            // A record of another file's, which no grouped writer may hold to: a writable open writes the record of
            // a mode that keeps no commit, and its own mode's after it
            if let (Some(file), Some(earlier)) = (file.as_mut().filter(|_| writable), record.as_ref()) {
                let void = record::of_mode(Some(earlier), Mode::Unsynced, boot);
                file.write(&void)?;
                record = Some(void);
            }
        }
        Outcome::Unchanged => {
            if let (Some(file), Some(earlier)) = (file.as_mut().filter(|_| writable), record.as_ref()) {
                let again = Record { boot, ..earlier.next(State::Kept) };
                file.write(&again)?;
                record = Some(again);
            }
        }
        Outcome::Restore => {
            let earlier = record.take().expect("a restore is of a record");
            let mut data = std::fs::OpenOptions::new().read(true).write(true).open(&data).map_err(|error| {
                pending(dir, &format!("its data file can't be written ({error}) and no writable open has made it"))
            })?;
            // The restore's records, which a read-only open that makes it writes too
            let mut own = None;
            let file = match file.as_mut() {
                Some(file) if writable => file,
                _ => own.insert(RecordFile::open(dir, true)?.expect("a record's file a writable open makes")),
            };
            // Its notice first, the rest of the record as it was: an open after a restore stopped once the meta pages
            // are written finds nothing left to restore, and still says one was made
            let intent = Record { restored: Some(earlier.commit), ..earlier.next(earlier.state) };
            file.write(&intent)?;
            restore(dir, &mut data, &intent)?;
            let again = Record { boot, ..intent.next(State::Kept) };
            file.write(&again)?;
            restored = again.restored;
            record = Some(again);
        }
    }
    // An open of another mode than the record's: the data synced, since a writer of the record's mode may have died
    // with commits unsynced, and then the record of this mode, before its first write
    if let (Some(mode), Some(file), Some(earlier)) = (mode, file.as_mut(), record.as_ref())
        && mode != Mode::Grouped
        && earlier.mode != mode
    {
        // Opened to write, which Windows' flush asks of the handle; a file that is gone holds nothing to sync
        if data.exists() {
            std::fs::OpenOptions::new().write(true).open(&data)?.sync_all()?;
        }
        let written = record::of_mode(Some(earlier), mode, boot);
        file.write(&written)?;
        record = Some(written);
    }
    let remembered =
        record.as_ref().filter(|record| record.mode == Mode::Grouped && record.state == State::Kept).map(|r| r.commit);
    // Taken while the lock every open decides under is held, and after any restore: a process that started after a
    // loss and has not restored yet is then never taken for one that holds what was written
    if alone {
        presence.unlock()?;
    }
    presence.lock_shared()?;
    Ok(Decided {
        presence,
        record: file.filter(|_| writable),
        remembered,
        restored,
        not_borne_out: outcome == Outcome::NotBorneOut,
    })
}

/// How an open that holds the database alone goes on, by a record of `data`
fn decided(data: &Path, record: &Record, boot: Boot) -> Result<Outcome> {
    if record.mode != Mode::Grouped {
        return Ok(Outcome::AsIs);
    }
    // A record whose data file has gone names nothing here
    if !data.exists() {
        return Ok(Outcome::NotBorneOut);
    }
    let mut file = File::open(data)?;
    let page = super::page_size_of(&mut file, data)?;
    let Some(newest) = reach::newer_meta_page(&mut file, page) else {
        return Err(reach::refused(data, "holds no meta page LMDB reads"));
    };
    let roots = record::roots_checksum(&mut file, page, reach::roots_of(&record.body))?;
    let borne_out = record.commit <= newest.txnid
        && (record.commit != newest.txnid || record.body == newest.body)
        && roots == record.roots;
    if !borne_out {
        if record.state == State::Failed {
            return Err(reach::refused(
                data,
                &format!(
                    "isn't the file the record beside it names, which a failed sync left to be restored to commit {}: \
                     the file may lack pages the sync didn't write. Restore the database from a backup",
                    record.commit
                ),
            ));
        }
        return Ok(Outcome::NotBorneOut);
    }
    Ok(match record.state {
        State::Failed => Outcome::Restore,
        // A process died, or closed: the OS holds what was written, and the newest commit is whole
        State::Kept if record.boot.same(&boot) => Outcome::AsIs,
        State::Kept if record.commit == newest.txnid => Outcome::Unchanged,
        // The machine stopped with commits made after the last sync, and what of them reached the disk is not known
        State::Kept => Outcome::Restore,
    })
}

/// The record's meta page written over both of the file's, the two as they were saved beside it first, and the file
/// synced: LMDB then opens at the record's commit, whose pages the pin kept as they were synced
fn restore(dir: &Path, data: &mut File, record: &Record) -> Result<()> {
    let page = super::page_size_of(data, &dir.join("data.mdb"))?;
    let saved = dir.join(record::RESTORED);
    // Written whole once: a save a death cut short is made again
    if std::fs::metadata(&saved).map_or(true, |saved| saved.len() < 2 * page as u64) {
        let mut metas = vec![0u8; 2 * page];
        data.seek(SeekFrom::Start(0))?;
        std::io::Read::read_exact(data, &mut metas)?;
        let mut file = File::create(&saved)?;
        file.write_all(&metas)?;
        file.sync_all()?;
        record::sync_directory(dir)?;
    }
    run_point!(RESTORE_SAVED at dir);
    for slot in 0..2u64 {
        // From the map's size on, as LMDB writes a meta page: the fields before it are the file's, written once
        data.seek(SeekFrom::Start(slot * page as u64 + (reach::PAGE_HEADER + record::WRITTEN_FROM) as u64))?;
        data.write_all(&record.body[record::WRITTEN_FROM..])?;
        if slot == 0 {
            run_point!(RESTORE_HALF at dir);
        }
    }
    run_point!(RESTORE_WRITTEN at dir);
    data.sync_all()?;
    run_point!(RESTORE_SYNCED at dir);
    Ok(())
}

/// An open refused for a restore it can't make, saying why and what to do
fn pending(dir: &Path, why: &str) -> Error {
    Error::new(
        ErrorCode::RestorePending,
        format!(
            "The database at {} is to be restored to its last synced commit after a power loss or a failed sync, and \
             {why}. Close the other processes that have it open, or open it writable once",
            dir.display()
        ),
    )
}
