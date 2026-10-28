//! `elmdb dump` and `elmdb load`: a database as a file of its schema and its primary data, written as the loads that
//! make it again (`docs/architecture/format.md`, Dumps). A dump names no key or stored value layout, so a database
//! crosses a change to format 1's layout by being dumped by the build before it and loaded by the build after.
//!
//! The file: the 8 bytes `ELMDBDMP`, a u32 dump version (1), a u32 length and the schema's canonical JSON (length 0:
//! none), then each load as a u32 length and its msgpack (`Load`, with named fields), then a u32 `0xFFFFFFFF` and a u64
//! load count. Integers are little-endian.

use std::io::{BufWriter, Read as _, Write as _};
use std::path::Path;

use serde::Serialize;

use crate::database::{Database, OpenOptions};
use crate::engine::maintenance::Load;
use crate::error::{Error, ErrorCode, Result};
use crate::schema::Schema;

const MAGIC: &[u8; 8] = b"ELMDBDMP";
const VERSION: u32 = 1;
const END: u32 = u32::MAX;
const LOAD_BATCH_BYTES: usize = 8 * 1024 * 1024;
const LOAD_BATCH_ROWS: usize = 1000;

/// What a dump holds
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DumpReport {
    pub schema: bool,
    pub entities: u64,
    pub fields: u64,
    pub relations: u64,
    pub embeddings: u64,
    pub sequences: u64,
}

impl DumpReport {
    fn count(&mut self, load: &Load) {
        match load {
            Load::Entity { fields, .. } => {
                self.entities += 1;
                self.fields += fields.len() as u64;
            }
            Load::Relation { .. } => self.relations += 1,
            Load::Embedding { .. } => self.embeddings += 1,
            Load::Sequence { .. } => self.sequences += 1,
            Load::Unset { .. } => {}
        }
    }
}

fn invalid(why: impl Into<String>) -> Error {
    Error::new(ErrorCode::WrongFormat, format!("The file isn't an elmdb dump this build reads: {}", why.into()))
}

/// Writes `db` to a new file at `target`: one moment of it, its schema and every piece of its primary data
pub fn dump(db: &Database, target: &Path) -> Result<DumpReport> {
    let file = std::fs::OpenOptions::new().write(true).create_new(true).open(target).map_err(|e| {
        if e.kind() == std::io::ErrorKind::AlreadyExists {
            Error::new(ErrorCode::IdInUse, format!("{} exists; a dump writes a new file", target.display()))
        } else {
            e.into()
        }
    })?;
    // A failed dump removes what it wrote, so no partial file passes for a dump
    let written = Written(Some(target.to_owned()));
    let mut out = BufWriter::new(file);
    let mut report = DumpReport::default();
    out.write_all(MAGIC)?;
    out.write_all(&VERSION.to_le_bytes())?;
    let mut count = 0u64;
    db.primary_snapshot(|schema, snapshot| {
        let schema = schema.map(|s| s.canonical()).unwrap_or_default();
        report.schema = !schema.is_empty();
        write_framed(&mut out, schema.as_bytes())?;
        crate::engine::maintenance::primary_loads_of(snapshot, |load| {
            report.count(&load);
            count += 1;
            let bytes = rmp_serde::to_vec_named(&load).expect("a load serializes");
            write_framed(&mut out, &bytes)
        })
    })?;
    out.write_all(&END.to_le_bytes())?;
    out.write_all(&count.to_le_bytes())?;
    out.into_inner().map_err(|e| Error::from(e.into_error()))?.sync_all()?;
    written.keep();
    Ok(report)
}

/// Reads the dump at `dump` into a new database at `target`, held to the dump's schema, and builds its derived data
pub fn load(dump: &Path, target: &Path) -> Result<DumpReport> {
    if target.join("data.mdb").exists() {
        return Err(Error::new(
            ErrorCode::IdInUse,
            format!("{} holds a database already; a load writes a new one", target.display()),
        ));
    }
    let mut file = std::io::BufReader::new(std::fs::File::open(dump)?);
    let mut head = [0u8; 12];
    file.read_exact(&mut head).map_err(|_| invalid("it ends inside its header"))?;
    if &head[..8] != MAGIC {
        return Err(invalid("it doesn't start with ELMDBDMP"));
    }
    let version = u32::from_le_bytes(head[8..].try_into().expect("4 bytes"));
    if version != VERSION {
        return Err(invalid(format!("it is dump version {version}, and this build reads {VERSION}")));
    }
    let schema_len = read_len(&mut file)?;
    let schema_text = read_bytes(&mut file, schema_len)?;
    let schema = if schema_text.is_empty() {
        None
    } else {
        let json: serde_json::Value =
            serde_json::from_slice(&schema_text).map_err(|e| invalid(format!("its schema doesn't parse: {e}")))?;
        Some(Schema::from_json(&json)?)
    };
    let mut report = DumpReport { schema: schema.is_some(), ..Default::default() };
    // Staging stays on the destination volume. Only the completed data file is published, without replacement.
    let target = std::path::absolute(target)?;
    #[cfg(unix)]
    let sync_dirs = crate::store::publication_directories(&target);
    std::fs::create_dir_all(&target)?;
    let target = target.canonicalize()?;
    let _reservation = crate::store::reserve_destination(&target)?;
    if target.join("data.mdb").exists() {
        return Err(Error::new(
            ErrorCode::IdInUse,
            "The load destination already holds a database; choose a new directory",
        ));
    }
    let stage = tempfile::Builder::new().prefix(".elmdb-load-").tempdir_in(&target)?;
    let db = Database::open(stage.path(), OpenOptions { schema, ..Default::default() })?;
    let mut loads = Vec::new();
    let mut batch_bytes = 0usize;
    let mut seen = 0u64;
    loop {
        let n = read_len(&mut file)?;
        if n == END {
            break;
        }
        let bytes = read_bytes(&mut file, n)?;
        let load: Load =
            rmp_serde::from_slice(&bytes).map_err(|e| invalid(format!("load {} doesn't decode: {e}", seen + 1)))?;
        report.count(&load);
        loads.push(load);
        seen += 1;
        batch_bytes = batch_bytes.saturating_add(n as usize);
        if batch_bytes >= LOAD_BATCH_BYTES || loads.len() >= LOAD_BATCH_ROWS {
            db.bulk_load(&loads)?;
            loads.clear();
            batch_bytes = 0;
        }
    }
    let mut count = [0u8; 8];
    file.read_exact(&mut count).map_err(|_| invalid("it ends before its load count"))?;
    if u64::from_le_bytes(count) != seen {
        return Err(invalid(format!("it records {} loads and holds {seen}", u64::from_le_bytes(count))));
    }
    if file.read(&mut [0u8; 1])? != 0 {
        return Err(invalid("it holds bytes after its load count"));
    }
    if !loads.is_empty() {
        db.bulk_load(&loads)?;
    }
    drop(loads);
    db.rebuild(None)?;
    db.close()?;
    run_point!(LOAD_PUBLISH);
    // Publication is atomic and fails if any file already occupies the destination. The closed staging file is
    // on the same volume; no failed operation ever removes a destination file owned by another invocation.
    crate::store::publish_new(&stage.path().join("data.mdb"), &target.join("data.mdb")).map_err(|error| {
        if error.kind() == std::io::ErrorKind::AlreadyExists {
            Error::new(ErrorCode::IdInUse, "The load destination now holds a database; choose a new directory")
        } else {
            error.into()
        }
    })?;
    let published = (|| -> Result<()> {
        failure_point!(LOAD_PUBLISHED, Error::new(ErrorCode::Io, "injected sync failure"));
        std::fs::OpenOptions::new().write(true).open(target.join("data.mdb"))?.sync_all()?;
        #[cfg(unix)]
        for dir in sync_dirs {
            std::fs::File::open(dir)?.sync_all()?;
        }
        Ok(())
    })();
    published.map_err(|error| {
        Error::new(
            ErrorCode::Io,
            format!(
                "The complete loaded database at {} is visible, but publication syncing failed: {error}",
                target.display()
            ),
        )
    })?;
    Ok(report)
}

fn write_framed(out: &mut impl std::io::Write, bytes: &[u8]) -> Result<()> {
    let n = u32::try_from(bytes.len()).ok().filter(|n| *n != END).ok_or_else(|| {
        Error::new(ErrorCode::ValueInvalid, format!("A load of {} bytes is past what a dump frames", bytes.len()))
    })?;
    out.write_all(&n.to_le_bytes())?;
    out.write_all(bytes)?;
    Ok(())
}

fn read_len(file: &mut impl std::io::Read) -> Result<u32> {
    let mut len = [0u8; 4];
    file.read_exact(&mut len).map_err(|_| invalid("it ends before its end marker"))?;
    Ok(u32::from_le_bytes(len))
}

/// `n` bytes of the dump, read as they arrive rather than allocated up front, so a damaged length fails as the dump
/// ending rather than as an allocation of up to 4 GiB
fn read_bytes(file: &mut impl std::io::Read, n: u32) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    file.take(u64::from(n)).read_to_end(&mut bytes)?;
    if bytes.len() != n as usize {
        return Err(invalid(format!("it ends inside a part {n} bytes long")));
    }
    Ok(bytes)
}

/// The files a dump or a load writes, removed unless it finishes
struct Written(Option<std::path::PathBuf>);

impl Written {
    fn keep(mut self) {
        self.0 = None;
    }
}

impl Drop for Written {
    fn drop(&mut self) {
        if let Some(target) = self.0.take() {
            if target.is_dir() {
                for file in ["data.mdb", "lock.mdb"] {
                    let _ = std::fs::remove_file(target.join(file));
                }
            } else {
                let _ = std::fs::remove_file(target);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::failpoints::{self, points};

    fn fixture(root: &Path) -> std::path::PathBuf {
        let db = Database::open(root.join("source"), OpenOptions { syncs: false, ..Default::default() }).unwrap();
        let loads: Vec<_> = (0..1100)
            .map(|i| Load::Entity {
                id: format!("Note-{i}"),
                fields: [("body".into(), rmpv::Value::from("x".repeat(9000)))].into(),
                trashed: false,
            })
            .collect();
        db.bulk_load(&loads).unwrap();
        let path = root.join("primary.dump");
        dump(&db, &path).unwrap();
        db.close().unwrap();
        path
    }

    #[test]
    fn staged_load_preserves_foreign_files_and_refuses_a_publication_race() {
        let root = tempfile::tempdir().unwrap();
        let path = fixture(root.path());
        let target = root.path().join("destination");
        std::fs::create_dir(&target).unwrap();
        std::fs::write(target.join("notes.txt"), b"unrelated").unwrap();
        let publishing = target.clone();
        failpoints::run_once_here(points::LOAD_PUBLISH, move || {
            assert!(!publishing.join("data.mdb").exists(), "staged batches are never public");
            assert!(matches!(
                Database::open(&publishing, OpenOptions::default()),
                Err(Error { code: ErrorCode::AlreadyOpen, .. })
            ));
            std::fs::write(publishing.join("data.mdb"), b"foreign publisher").unwrap();
        });
        assert_eq!(load(&path, &target).unwrap_err().code, ErrorCode::IdInUse);
        assert_eq!(std::fs::read(target.join("data.mdb")).unwrap(), b"foreign publisher");
        assert_eq!(std::fs::read(target.join("notes.txt")).unwrap(), b"unrelated");
        assert_eq!(std::fs::read_dir(&target).unwrap().count(), 2, "private staging cleaned up");
        std::fs::remove_file(target.join("data.mdb")).unwrap();
        let report = load(&path, &target).unwrap();
        assert_eq!(report.entities, 1100);
        let db = Database::open(&target, OpenOptions { syncs: false, ..Default::default() }).unwrap();
        let again = root.path().join("again.dump");
        dump(&db, &again).unwrap();
        assert_eq!(std::fs::read(again).unwrap(), std::fs::read(path).unwrap());
        assert!(db.check().unwrap().is_clean());
    }

    #[test]
    fn unwinding_before_publication_cleans_staging_and_postpublication_failure_keeps_the_database() {
        let root = tempfile::tempdir().unwrap();
        let path = fixture(root.path());
        let target = root.path().join("destination");
        failpoints::run_once_here(points::LOAD_PUBLISH, || panic!("cancel import"));
        assert!(std::panic::catch_unwind(|| load(&path, &target)).is_err());
        assert_eq!(std::fs::read_dir(&target).unwrap().count(), 0);
        failpoints::fail_once_here(points::LOAD_PUBLISHED);
        let error = load(&path, &target).unwrap_err();
        assert_eq!(error.code, ErrorCode::Io);
        assert!(error.message.contains("complete loaded database") && error.message.contains("visible"));
        let db = Database::open(&target, OpenOptions { syncs: false, ..Default::default() }).unwrap();
        assert!(db.check().unwrap().is_clean());
        let mut entities = 0;
        db.primary_loads(|load| {
            if matches!(load, Load::Entity { .. }) {
                entities += 1;
            }
            Ok(())
        })
        .unwrap();
        assert_eq!(entities, 1100);
        assert_eq!(load(&path, &target).unwrap_err().code, ErrorCode::IdInUse);
    }

    #[test]
    fn late_invalid_input_cleans_staged_batches_and_can_be_retried() {
        let root = tempfile::tempdir().unwrap();
        let path = fixture(root.path());
        let bytes = std::fs::read(&path).unwrap();
        let target = root.path().join("destination");
        let bad = root.path().join("bad.dump");
        for kind in ["count", "truncated", "trailing"] {
            let mut broken = bytes.clone();
            match kind {
                "count" => {
                    let at = broken.len() - 8;
                    broken[at..].copy_from_slice(&0u64.to_le_bytes());
                }
                "truncated" => {
                    broken.truncate(broken.len() - 1);
                }
                _ => broken.push(0),
            }
            std::fs::write(&bad, broken).unwrap();
            assert_eq!(load(&bad, &target).unwrap_err().code, ErrorCode::WrongFormat, "{kind}");
            assert_eq!(std::fs::read_dir(&target).unwrap().count(), 0, "{kind}: staging cleaned up");
        }
        assert_eq!(load(&path, &target).unwrap().entities, 1100);
    }
}
