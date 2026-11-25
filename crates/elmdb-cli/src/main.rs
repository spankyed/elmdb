//! The `elmdb` command: inspect, check, rebuild, back up, dump and load databases. Each opens the database itself, so the app
//! that holds one open must close it first, except for `inspect`, `check`, `backup` and `dump`, which open it read-only.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use elmdb::{Database, OpenOptions};

#[derive(Parser)]
#[command(name = "elmdb", version, about = "Inspect, check, rebuild, back up, dump and load elmdb databases")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// What a database holds: entities per type and field, relations per kind, entries per index and the bytes of
    /// each value index, its sizes
    Inspect { dir: PathBuf },
    /// Compares derived data with primary data and every row with the schema's rules; exits 1 when it finds anything
    Check { dir: PathBuf },
    /// Deletes every index's derived data and writes what primary data derives, in one durable unit: what a database
    /// an older elmdb built needs once its format changed derived data in place. Opens the database to write
    Rebuild { dir: PathBuf },
    /// A compacted copy of a database into a new directory, an empty one, or an earlier backup, which it replaces
    Backup { dir: PathBuf, target: PathBuf },
    /// A database's schema and primary data into a new file, which `load` makes a database of again
    Dump { dir: PathBuf, target: PathBuf },
    /// A dump into a new database, held to the dump's schema, its derived data built
    Load { dump: PathBuf, target: PathBuf },
    /// Says the restore an open reported was read, so that no open reports it after. Opens the database to write
    AcknowledgeRestore { dir: PathBuf },
}

fn read_only(dir: &PathBuf) -> elmdb::Result<Database> {
    told(Database::open(dir, OpenOptions { read_only: true, ..Default::default() })?)
}

/// An existing database, opened to write. A writable open makes a database where there is none, which a command given a
/// mistyped directory must not do, so a read-only open goes first: it refuses a directory that holds none
/// (`NO_DATABASE`), and closes before the writable one opens
fn existing(dir: &PathBuf) -> elmdb::Result<Database> {
    Database::open(dir, OpenOptions { read_only: true, ..Default::default() })?.close()?;
    told(Database::open(dir, OpenOptions::default())?)
}

/// The database, its open having said on the error stream what its caller is to hear: a restore no caller has said it
/// read, which every open reports until one does, and a record that named another file
fn told(db: Database) -> elmdb::Result<Database> {
    let report = db.open_report();
    if let Some(restored) = report.restored {
        let at =
            restored.sequence.map_or_else(|| "at a sequence not yet read".to_owned(), |n| format!("at sequence {n}"));
        eprintln!(
            "Restored: a power loss or a failed sync took the database back to its last synced commit, which it opened \
             {at}; the commits after it are gone. `elmdb acknowledge-restore DIR` says this was read"
        );
    }
    if report.not_borne_out {
        eprintln!("The record beside the data file named another file than this one, which was opened as it is");
    }
    Ok(db)
}

/// Refuses a backup target that is a file, or a directory holding a database an open has written in place or anything
/// else no backup leaves: the command copies into a new directory, an empty one or an earlier backup, which it
/// replaces, and never over what a target named in the wrong place holds. The engine refuses one another process has
/// open
fn check_backup_target(target: &Path) -> elmdb::Result<()> {
    let meta = match std::fs::metadata(target) {
        Ok(meta) => meta,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(elmdb::Error::new(
                elmdb::ErrorCode::Io,
                format!("Cannot look at {}: {error}", target.display()),
            ));
        }
    };
    if !meta.is_dir() {
        return Err(elmdb::Error::new(
            elmdb::ErrorCode::Io,
            format!("{} is a file; back up into a new directory, an empty one or an earlier backup", target.display()),
        ));
    }
    if elmdb::written_in(target)? {
        return Err(elmdb::Error::new(
            elmdb::ErrorCode::TargetInUse,
            format!(
                "{} holds a database that has been opened for writing, which a backup would replace; back up into a \
                 new directory, an empty one or an earlier backup",
                target.display()
            ),
        ));
    }
    let mut has_lmdb = false;
    let mut has_record_lock = false;
    for entry in std::fs::read_dir(target)? {
        let entry = entry?;
        let name = entry.file_name();
        let file_type = entry.file_type()?;
        // Finder's, beside anything a person has looked at on macOS
        if !elmdb::left_by_a_backup(&name) && name != ".DS_Store" {
            return Err(elmdb::Error::new(
                elmdb::ErrorCode::TargetInUse,
                format!(
                    "{} holds {}, which no backup leaves, so it may not be a backup; back up into a new directory, an \
                     empty one or an earlier backup",
                    target.display(),
                    name.to_string_lossy()
                ),
            ));
        }
        // `wake` is a directory holding listeners' mailboxes; everything else a backup or Finder leaves is a regular
        // file, never a directory or a symbolic link
        let expected_type = if name == "wake" { file_type.is_dir() } else { file_type.is_file() };
        if !expected_type {
            return Err(elmdb::Error::new(
                elmdb::ErrorCode::TargetInUse,
                format!(
                    "{} holds {} with an unexpected file type, which no backup leaves; back up into a new directory, \
                     an empty one or an earlier backup",
                    target.display(),
                    name.to_string_lossy()
                ),
            ));
        }
        if name == "data.mdb" || name == "lock.mdb" {
            has_lmdb = true;
        } else if elmdb::is_backup_lock(&name) {
            has_record_lock = true;
        }
    }
    if has_lmdb && !has_record_lock {
        return Err(elmdb::Error::new(
            elmdb::ErrorCode::TargetInUse,
            format!(
                "{} holds LMDB files without an elmdb backup lock (synced.lock), so it may belong to another program; \
                 back up into a new directory, an empty one or an earlier backup",
                target.display()
            ),
        ));
    }
    Ok(())
}

fn run(command: Command) -> elmdb::Result<bool> {
    match command {
        Command::Inspect { dir } => {
            let stats = read_only(&dir)?.stats()?;
            println!("{}", serde_json::to_string_pretty(&stats).expect("stats serialize"));
            Ok(true)
        }
        Command::Check { dir } => {
            let report = read_only(&dir)?.check()?;
            println!("{}", serde_json::to_string_pretty(&report).expect("a report serializes"));
            Ok(report.is_clean())
        }
        Command::Rebuild { dir } => {
            let db = existing(&dir)?;
            db.rebuild(None)?;
            db.close()?;
            println!("Rebuilt every index of {}", dir.display());
            Ok(true)
        }
        Command::Backup { dir, target } => {
            let db = read_only(&dir)?;
            // Once the source is open, which can take a while, so that the target is looked at as close to the backup
            // as can be
            check_backup_target(&target)?;
            db.backup(&target)?;
            println!("{} copied to {}", dir.display(), target.display());
            Ok(true)
        }
        Command::Dump { dir, target } => {
            let report = elmdb::dump::dump(&read_only(&dir)?, &target)?;
            println!("{}", serde_json::to_string_pretty(&report).expect("a report serializes"));
            Ok(true)
        }
        Command::Load { dump, target } => {
            let report = elmdb::dump::load(&dump, &target)?;
            println!("{}", serde_json::to_string_pretty(&report).expect("a report serializes"));
            Ok(true)
        }
        Command::AcknowledgeRestore { dir } => {
            let db = existing(&dir)?;
            let had = db.open_report().restored.is_some();
            db.acknowledge_restore()?;
            db.close()?;
            println!(
                "{}",
                if had { "Acknowledged: no open reports the restore now" } else { "No restore to acknowledge" }
            );
            Ok(true)
        }
    }
}

fn main() -> ExitCode {
    match run(Cli::parse().command) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(1),
        Err(error) => {
            eprintln!("{error}");
            ExitCode::from(2)
        }
    }
}
