//! The record beside the data file (`docs/roadmap/synced-commit.md`): the last commit the grouped mode knows to be on
//! disk, or the mode that keeps no such commit, in `synced`, and the files whose locks say who has the database open and
//! who is reading or writing the record.
//!
//! The record is two slots written in turn, in place, each a block of its own that ends in a checksum of what it holds:
//! the slot of the higher count that reads is the record. Writing one slot and syncing it is one sync for each record,
//! and a write that a power loss tears leaves the other slot standing.
//!
//! A lock is a file of its own, never written and locked whole: a lock on a range of a file is the process's on unix
//! and goes when it closes any descriptor of that file, and on Windows keeps other processes from reading the range.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use super::boot::Boot;
use crate::error::{Error, ErrorCode, Result};

/// The record's file, beside `data.mdb`
pub(crate) const RECORD: &str = "synced";
/// The two meta pages a restore wrote over, as they were, written once
pub(super) const RESTORED: &str = "synced.restored";
/// Locked by every open of the database, shared, for as long as it is open: a database another process has open is
/// in this boot
pub(super) const PRESENCE: &str = "open.lock";
/// Locked by the one reading or writing the record, and by an open while it decides what to open at
pub(super) const RECORD_LOCK: &str = "synced.lock";
/// Locked shared by every writable open of the mode, for as long as it is open: a writable open of another mode is
/// refused while one of these is held
pub(super) const fn mode_lock(mode: Mode) -> &'static str {
    match mode {
        Mode::Grouped => "grouped.lock",
        Mode::Ordered => "ordered.lock",
        Mode::Unsynced => "unsynced.lock",
    }
}

/// Whether `dir` holds a database an open has written in place: the record, or a mode's lock, which every writable open
/// leaves and a backup never makes. A backup a read-only open has read leaves neither
pub fn written_in(dir: &Path) -> bool {
    std::iter::once(RECORD).chain(Mode::ALL.map(mode_lock)).any(|name| dir.join(name).exists())
}

/// The bytes of LMDB's `MDB_meta`, which follow a meta page's header: the magic, the version, the fixed address, the
/// map's size, the free and main trees, the last page and the transaction that committed it
pub(super) const BODY: usize = 136;
/// Where LMDB starts writing a meta page's body, the map's size: the fields before it are written once, with the file
pub(super) const WRITTEN_FROM: usize = 16;

/// Each slot is a block of this many bytes, so that no write of one touches the block of the other
const SLOT: usize = 4096;
const MAGIC: &[u8; 8] = b"elmdbsyn";
const VERSION: u32 = 1;
/// The magic, the version, the count, the mode, the state, the commit, the body, the roots' checksum, the boot, and
/// the notice's flag and commit
const FIELDS: usize = 8 + 4 + 8 + 1 + 1 + 8 + BODY + 32 + 32 + 1 + 8;
const CHECKSUM: usize = 32;

/// How the database is written
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Mode {
    /// Synced in groups, keeping the last synced commit by a pin and this record
    Grouped,
    /// Each commit synced as LMDB makes it, in LMDB's own order of writes (`syncEachCommit`)
    Ordered,
    /// Never synced (`syncs: false`): nothing is known of what a loss leaves
    Unsynced,
}

impl Mode {
    pub(super) const ALL: [Mode; 3] = [Mode::Grouped, Mode::Ordered, Mode::Unsynced];

    /// The mode as a caller opens in it
    pub(super) const fn as_opened(self) -> &'static str {
        match self {
            Mode::Grouped => "grouped mode (the default)",
            Mode::Ordered => "`syncEachCommit` mode",
            Mode::Unsynced => "`syncs: false` mode",
        }
    }
}

/// Whether the grouped mode's syncs have held
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum State {
    Kept,
    /// A sync failed: what was written since the record's commit may not be on disk, and only a restore writes over
    /// this
    Failed,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub(super) struct Record {
    /// Records written to this file, this one included: the slot it is in is the count's parity
    pub count: u64,
    pub mode: Mode,
    pub state: State,
    /// The LMDB transaction whose commit is on disk, under the grouped mode
    pub commit: u64,
    /// That commit's meta page body
    pub body: [u8; BODY],
    /// A checksum of the root pages the body names, and of the named databases' roots (`roots_checksum`)
    pub roots: [u8; 32],
    /// The boot it was written in
    pub boot: Boot,
    /// A restore was made, and no caller has said it read so
    pub restored: Option<Restored>,
}

/// What a restore leaves for a caller to read, until one says it has
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Restored {
    /// The LMDB commit the restore took the file to, until the open that made it reads that commit's sequence
    Commit(u64),
    /// The sequence the database opened at, which a caller knows its commits by
    Sequence(u64),
}

impl Record {
    fn encode(&self) -> [u8; SLOT] {
        let mut slot = [0u8; SLOT];
        let mut at = 0;
        let mut put = |bytes: &[u8]| {
            slot[at..at + bytes.len()].copy_from_slice(bytes);
            at += bytes.len();
        };
        put(MAGIC);
        put(&VERSION.to_le_bytes());
        put(&self.count.to_le_bytes());
        put(&[match self.mode {
            Mode::Grouped => 1,
            Mode::Ordered => 2,
            Mode::Unsynced => 3,
        }]);
        put(&[match self.state {
            State::Kept => 1,
            State::Failed => 2,
        }]);
        put(&self.commit.to_le_bytes());
        put(&self.body);
        put(&self.roots);
        put(self.boot.bytes());
        let (noticed, restored) = match self.restored {
            None => (0, 0),
            Some(Restored::Commit(commit)) => (1, commit),
            Some(Restored::Sequence(sequence)) => (2, sequence),
        };
        put(&[noticed]);
        put(&restored.to_le_bytes());
        debug_assert_eq!(at, FIELDS);
        let checksum = Sha256::digest(&slot[..FIELDS]);
        slot[FIELDS..FIELDS + CHECKSUM].copy_from_slice(&checksum);
        slot
    }

    /// The record a slot holds, if it was written whole by this version
    fn decode(slot: &[u8]) -> Option<Self> {
        let slot = slot.get(..FIELDS + CHECKSUM)?;
        if Sha256::digest(&slot[..FIELDS]).as_slice() != &slot[FIELDS..] || &slot[..8] != MAGIC {
            return None;
        }
        let mut at = 8;
        let mut take = |len: usize| {
            let bytes = &slot[at..at + len];
            at += len;
            bytes
        };
        let u64_of = |bytes: &[u8]| u64::from_le_bytes(bytes.try_into().expect("eight bytes"));
        if u32::from_le_bytes(take(4).try_into().expect("four bytes")) != VERSION {
            return None;
        }
        let count = u64_of(take(8));
        let mode = match take(1)[0] {
            1 => Mode::Grouped,
            2 => Mode::Ordered,
            3 => Mode::Unsynced,
            _ => return None,
        };
        let state = match take(1)[0] {
            1 => State::Kept,
            2 => State::Failed,
            _ => return None,
        };
        let commit = u64_of(take(8));
        let body = take(BODY).try_into().expect("a body's bytes");
        let roots = take(32).try_into().expect("a checksum's bytes");
        let boot = Boot::from_bytes(take(32).try_into().expect("a boot's bytes"));
        let noticed = take(1)[0];
        let value = u64_of(take(8));
        // A value this version doesn't know is a later one's notice, read as none, as a version that knows only 0 and 1
        // reads 2
        let restored = match noticed {
            1 => Some(Restored::Commit(value)),
            2 => Some(Restored::Sequence(value)),
            _ => None,
        };
        Some(Self { count, mode, state, commit, body, roots, boot, restored })
    }

    /// This record written again as the next, in `state`: what an open or a sync writes over it
    pub(super) fn next(&self, state: State) -> Self {
        Self { count: self.count + 1, state, ..self.clone() }
    }
}

/// A record of a mode that keeps no commit, the first a file holds when `earlier` is none
pub(super) fn of_mode(earlier: Option<&Record>, mode: Mode, boot: Boot) -> Record {
    Record {
        count: earlier.map_or(1, |record| record.count + 1),
        mode,
        state: State::Kept,
        commit: 0,
        body: [0; BODY],
        roots: [0; 32],
        boot,
        restored: earlier.and_then(|record| record.restored),
    }
}

/// The record's file, opened for reading and, by a writable open, for writing
pub(super) struct RecordFile {
    file: File,
    path: PathBuf,
}

impl RecordFile {
    /// The record's file in `dir`, made by a writable open, and its directory synced when it was. None where a
    /// read-only open finds no record
    pub(super) fn open(dir: &Path, writable: bool) -> Result<Option<Self>> {
        let path = dir.join(RECORD);
        if !writable {
            return match File::open(&path) {
                Ok(file) => Ok(Some(Self { file, path })),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(error) => Err(error.into()),
            };
        }
        let file = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(false).open(&path)?;
        // Sync the directory whenever the file is empty — both when just created and when a prior crash left a
        // 0-byte record file before its directory entry was synced
        // mutant-skip: replace == with != in RecordFile::open
        // What it changes is whether the directory is synced when the file is empty, which only a loss tells apart
        if file.metadata()?.len() == 0 {
            sync_directory(dir)?;
        }
        Ok(Some(Self { file, path }))
    }

    /// The record: the slot of the higher count that reads. None where neither slot was ever written whole, which is
    /// a record never made durable; refused where both were written and neither reads
    pub(super) fn read(&mut self) -> Result<Option<Record>> {
        let mut bytes = Vec::with_capacity(2 * SLOT);
        self.file.seek(SeekFrom::Start(0))?;
        (&self.file).take(2 * SLOT as u64).read_to_end(&mut bytes)?;
        let slots: Vec<&[u8]> = bytes.chunks(SLOT).collect();
        let written = slots.iter().filter(|slot| slot.iter().any(|byte| *byte != 0)).count();
        let records: Vec<Record> = slots.iter().filter_map(|slot| Record::decode(slot)).collect();
        // A slot another version of elmdb wrote, whose record this one can't read
        let other_version =
            slots.iter().any(|slot| slot.len() >= 12 && &slot[..8] == MAGIC && slot[8..12] != VERSION.to_le_bytes());
        match records.into_iter().max_by_key(|record| record.count) {
            Some(record) => Ok(Some(record)),
            // The first record's write, torn by a loss before it was synced: there was no record on disk
            None if written < 2 || other_version => Ok(None),
            None => Err(Error::new(
                ErrorCode::Corrupted,
                format!(
                    "{} holds two records and neither reads, where each is written whole and one always stands: \
                     something other than elmdb wrote it. Move it aside to open the database as its data file is",
                    self.path.display()
                ),
            )),
        }
    }

    /// Writes `record` to its slot and syncs it
    pub(super) fn write(&mut self, record: &Record) -> Result<()> {
        self.file.seek(SeekFrom::Start(record.count % 2 * SLOT as u64))?;
        self.file.write_all(&record.encode())?;
        run_point!(RECORD_WRITTEN at self.path.parent().expect("the record is in a directory"));
        self.sync()
    }

    /// Syncs what the file holds: a record read is relied on only once it is synced, since its writer may have died
    /// before syncing it
    pub(super) fn sync(&mut self) -> Result<()> {
        // mutant-skip: replace RecordFile::sync -> Result<\(\)> with Ok\(\(\)\)
        // A record unsynced reads as one synced until the machine loses power, which no test here can make
        self.file.sync_all().map_err(|error| {
            Error::new(ErrorCode::Io, format!("Couldn't sync the record {}: {error}", self.path.display()))
        })
    }
}

/// One of the database's lock files, made where it is missing. A read-only open makes it too: where it can't, on a
/// filesystem it can't write, it is refused as it is where it can't make LMDB's
pub(super) fn lock_file(dir: &Path, name: &str) -> Result<File> {
    let path = dir.join(name);
    let opened = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(false).open(&path);
    let file = match opened {
        Ok(file) => file,
        // A lock file this process may read and not write, another user's or on a filesystem mounted read-only:
        // read, it is locked all the same, and the lock is all it is for
        Err(_) if path.exists() => File::open(&path)?,
        Err(error) => {
            return Err(Error::new(
                ErrorCode::Io,
                format!(
                    "Cannot make the lock file {}: {error}. Open the database on a filesystem this process can \
                     write, or use a writable copy",
                    path.display()
                ),
            ));
        }
    };
    Ok(file)
}

/// The record's lock between processes, let go when dropped
pub(super) struct Locked<'a>(&'a File);

impl Drop for Locked<'_> {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

/// Takes `file`'s lock whole and exclusive, waiting for it
pub(super) fn exclusive(file: &File) -> Result<Locked<'_>> {
    file.lock()?;
    Ok(Locked(file))
}

/// Syncs a directory, so a file made in it is there after a loss. Windows syncs no directory, and its filesystem
/// journals what a directory holds
pub(super) fn sync_directory(dir: &Path) -> Result<()> {
    // mutant-skip: replace sync_directory -> Result<\(\)> with Ok\(\(\)\)
    // A file's name unsynced reads as one synced until the machine loses power, which no test here can make
    #[cfg(unix)]
    File::open(dir)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = dir;
    Ok(())
}

/// A checksum of a commit's root pages as `file` holds them, of `page` bytes each: the free tree's and the main tree's
/// at `roots`, and the root of each named database the main tree's root lists, elmdb's among them, whose page a
/// commit that writes a row writes again. A root of `u64::MAX` is an empty tree's, and a page past the end of the
/// file reads as none
pub(super) fn roots_checksum(file: &mut File, page: usize, roots: [u64; 2]) -> Result<[u8; 32]> {
    let mut hash = Sha256::new();
    let mut read = |root: u64, hash: &mut Sha256| -> Result<Option<Vec<u8>>> {
        hash.update(root.to_le_bytes());
        if root == u64::MAX {
            return Ok(None);
        }
        // A root read from another file's page may be any number: one past what a file can hold reads as none
        let Some(at) = root.checked_mul(page as u64) else {
            hash.update(b"past the end");
            return Ok(None);
        };
        let mut bytes = vec![0u8; page];
        file.seek(SeekFrom::Start(at))?;
        match file.read_exact(&mut bytes) {
            Ok(()) => {
                hash.update(&bytes);
                Ok(Some(bytes))
            }
            Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => {
                hash.update(b"past the end");
                Ok(None)
            }
            Err(error) => Err(error.into()),
        }
    };
    read(roots[0], &mut hash)?;
    if let Some(main) = read(roots[1], &mut hash)? {
        for named in super::reach::named_roots(&main) {
            read(named, &mut hash)?;
        }
    }
    Ok(hash.finalize().into())
}

/// A record whose boot is none, which is another boot than any: what a power loss replayed on files is opened as
#[cfg(test)]
pub(crate) fn of_another_boot(bytes: &[u8]) -> Vec<u8> {
    let mut bytes = bytes.to_vec();
    for slot in bytes.chunks_mut(SLOT) {
        if let Some(mut record) = Record::decode(slot) {
            record.boot = Boot::none();
            let len = slot.len().min(SLOT);
            slot[..len].copy_from_slice(&record.encode()[..len]);
        }
    }
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(count: u64) -> Record {
        Record {
            count,
            mode: Mode::Grouped,
            state: State::Kept,
            commit: 40 + count,
            body: [7; BODY],
            roots: [9; 32],
            boot: Boot::none(),
            restored: Some(Restored::Commit(3)),
        }
    }

    #[test]
    fn a_record_reads_back_as_written_and_the_slot_of_the_higher_count_is_the_record() {
        let dir = tempfile::tempdir().unwrap();
        let mut file = RecordFile::open(dir.path(), true).unwrap().unwrap();
        assert_eq!(file.read().unwrap(), None, "a file just made holds no record");
        file.write(&record(1)).unwrap();
        assert_eq!(file.read().unwrap(), Some(record(1)));
        file.write(&record(2)).unwrap();
        assert_eq!(file.read().unwrap(), Some(record(2)));
        file.write(&record(3)).unwrap();
        assert_eq!(file.read().unwrap(), Some(record(3)));
        // Each in the slot of its count's parity: the second is still in the other
        let bytes = std::fs::read(dir.path().join(RECORD)).unwrap();
        assert_eq!(Record::decode(&bytes[..SLOT]), Some(record(2)));
        assert_eq!(Record::decode(&bytes[SLOT..]), Some(record(3)));
    }

    #[test]
    fn every_mode_and_state_reads_back_as_written() {
        for mode in [Mode::Grouped, Mode::Ordered, Mode::Unsynced] {
            for state in [State::Kept, State::Failed] {
                let written = Record { mode, state, ..record(1) };
                assert_eq!(Record::decode(&written.encode()), Some(written));
            }
        }
    }

    #[test]
    fn a_record_of_a_mode_follows_the_one_before_it_and_keeps_its_notice() {
        let next = of_mode(Some(&record(5)), Mode::Ordered, Boot::none());
        assert_eq!(
            (next.count, next.mode, next.commit, next.restored),
            (6, Mode::Ordered, 0, Some(Restored::Commit(3)))
        );
        assert_eq!(of_mode(None, Mode::Unsynced, Boot::none()).count, 1);
    }

    #[cfg(unix)]
    #[test]
    fn a_read_only_open_finds_no_record_where_there_is_none_and_refuses_one_it_cannot_open() {
        let dir = tempfile::tempdir().unwrap();
        assert!(RecordFile::open(dir.path(), false).unwrap().is_none());
        // A directory that is a file: the record's path can't be opened, and not because nothing is there
        let not_a_directory = dir.path().join("a file");
        std::fs::write(&not_a_directory, b"").unwrap();
        assert!(RecordFile::open(&not_a_directory, false).is_err());
    }

    #[test]
    fn a_slot_another_version_wrote_beside_a_torn_one_is_no_record_and_two_torn_slots_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let mut file = RecordFile::open(dir.path(), true).unwrap().unwrap();
        // One slot of a later version, whose record this one can't read, and the other written but torn
        let mut later = [7u8; SLOT];
        later[..8].copy_from_slice(MAGIC);
        later[8..12].copy_from_slice(&(VERSION + 1).to_le_bytes());
        std::fs::write(dir.path().join(RECORD), [later, [9u8; SLOT]].concat()).unwrap();
        assert_eq!(file.read().unwrap(), None);
        // Both torn, and neither of another version: something else wrote it
        std::fs::write(dir.path().join(RECORD), [[7u8; SLOT], [9u8; SLOT]].concat()).unwrap();
        assert_eq!(file.read().unwrap_err().code, ErrorCode::Corrupted);
    }

    #[test]
    fn a_lock_file_that_can_t_be_made_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing");
        let refused = lock_file(&missing, PRESENCE).unwrap_err();
        assert!(refused.message.starts_with("Cannot make the lock file"), "{refused}");
    }

    #[test]
    fn a_root_past_the_file_s_end_reads_as_none_and_one_that_can_t_be_read_fails_the_checksum() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data");
        std::fs::write(&path, [3u8; 512]).unwrap();
        let mut file = File::open(&path).unwrap();
        // The page past the end, beside an empty tree: as none, and not as the page that is there
        let past = roots_checksum(&mut file, 512, [1, u64::MAX]).unwrap();
        assert_ne!(past, roots_checksum(&mut file, 512, [0, u64::MAX]).unwrap());
        #[cfg(unix)]
        {
            // A directory opens and fails every read
            let mut directory = File::open(dir.path()).unwrap();
            assert!(roots_checksum(&mut directory, 512, [0, u64::MAX]).is_err());
        }
    }

    #[test]
    fn a_notice_of_a_kind_this_version_doesn_t_know_reads_as_none() {
        let mut slot = Record { restored: Some(Restored::Sequence(9)), ..record(1) }.encode();
        // The notice's byte, before its u64 at the fields' end, made a later version's, and the checksum made again
        slot[FIELDS - 9] = 3;
        let checksum = Sha256::digest(&slot[..FIELDS]);
        slot[FIELDS..FIELDS + CHECKSUM].copy_from_slice(&checksum);
        assert_eq!(Record::decode(&slot), Some(Record { restored: None, ..record(1) }));
    }

    #[test]
    fn a_torn_slot_leaves_the_other_and_two_torn_slots_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let mut file = RecordFile::open(dir.path(), true).unwrap().unwrap();
        file.write(&record(1)).unwrap();
        file.write(&record(2)).unwrap();
        let path = dir.path().join(RECORD);
        let mut bytes = std::fs::read(&path).unwrap();
        // The second record's slot, the first block, torn part way through its body
        bytes[100] ^= 0xff;
        std::fs::write(&path, &bytes).unwrap();
        assert_eq!(file.read().unwrap(), Some(record(1)));
        bytes[SLOT + 100] ^= 0xff;
        std::fs::write(&path, &bytes).unwrap();
        assert_eq!(file.read().unwrap_err().code, ErrorCode::Corrupted);
    }

    #[test]
    fn a_first_record_torn_before_its_sync_is_no_record() {
        let dir = tempfile::tempdir().unwrap();
        let mut file = RecordFile::open(dir.path(), true).unwrap().unwrap();
        file.write(&record(1)).unwrap();
        let path = dir.path().join(RECORD);
        let mut bytes = std::fs::read(&path).unwrap();
        // The first record is in the slot of an odd count, the second block
        bytes[SLOT + 100] ^= 0xff;
        std::fs::write(&path, &bytes).unwrap();
        assert_eq!(file.read().unwrap(), None);
    }

    #[test]
    fn a_record_of_another_boot_is_the_same_record_but_its_boot() {
        let dir = tempfile::tempdir().unwrap();
        let mut file = RecordFile::open(dir.path(), true).unwrap().unwrap();
        let mut written = record(1);
        written.boot = Boot::of(b"a boot");
        file.write(&written).unwrap();
        let path = dir.path().join(RECORD);
        std::fs::write(&path, of_another_boot(&std::fs::read(&path).unwrap())).unwrap();
        assert_eq!(file.read().unwrap(), Some(Record { boot: Boot::none(), ..written }));
    }
}
