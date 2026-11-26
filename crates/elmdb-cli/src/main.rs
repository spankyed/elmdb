//! The `elmdb` command: inspect, check, rebuild, back up, dump, load, and purge archived types.
//! Each opens the database itself; `inspect`, `check`, `backup` and `dump` open it read-only.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use elmdb::{Database, OpenOptions};

#[derive(Parser)]
#[command(name = "elmdb", version, about = "Inspect, check, rebuild, back up, dump, load and purge elmdb v2 databases")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Prints database stats: schema epoch, commit sequence, entity counts per type, index count, sizes
    Inspect { dir: PathBuf },
    /// Compares every derived index projection against primary state; exits 1 if any mismatch is found
    Check {
        dir: PathBuf,
        /// Optional single index name to check
        #[arg(long)]
        index: Option<String>,
    },
    /// Rebuilds every derived index (or one named index) cleanly from primary state
    Rebuild {
        dir: PathBuf,
        /// Optional single index name to rebuild
        #[arg(long)]
        index: Option<String>,
    },
    /// Creates a live compacted copy into a new, empty, or previously backed-up directory
    Backup { dir: PathBuf, target: PathBuf },
    /// Exports schema and primary data (entities, relations, embeddings) to a new JSON file
    Dump { dir: PathBuf, target: PathBuf },
    /// Loads a dump file into a new database and rebuilds all derived indexes
    Load { dump: PathBuf, target: PathBuf },
    /// Syncs and updates the dual-slot durability sidecar to acknowledge an unsynced tail
    AcknowledgeRestore { dir: PathBuf },
    /// Permanently destroys all entities of an archived type
    Purge { dir: PathBuf, type_name: String },
}

fn read_only(dir: &PathBuf) -> elmdb::Result<Database> {
    told(Database::open(dir, OpenOptions { read_only: true, ..Default::default() })?)
}

fn existing(dir: &PathBuf) -> elmdb::Result<Database> {
    Database::open(dir, OpenOptions { read_only: true, ..Default::default() })?.close()?;
    told(Database::open(dir, OpenOptions::default())?)
}

fn told(db: Database) -> elmdb::Result<Database> {
    let report = db.open_report();
    if report.status == "unsyncedAfterReboot" {
        eprintln!(
            "Warning: database opened with status '{}' at txnid {}; run `elmdb acknowledge-restore DIR` after verifying state.",
            report.status, report.observed_txnid
        );
    }
    Ok(db)
}

fn run(command: Command) -> elmdb::Result<bool> {
    match command {
        Command::Inspect { dir } => {
            let db = read_only(&dir)?;
            let stats = db.stats()?;
            db.close()?;
            println!("{}", serde_json::to_string_pretty(&stats).expect("stats serialize"));
            Ok(true)
        }
        Command::Check { dir, index } => {
            let db = read_only(&dir)?;
            let report = db.check(index.as_deref())?;
            db.close()?;
            println!("{}", serde_json::to_string_pretty(&report).expect("report serializes"));
            Ok(report.is_clean())
        }
        Command::Rebuild { dir, index } => {
            let db = existing(&dir)?;
            db.rebuild(index.as_deref())?;
            db.close()?;
            println!("Rebuilt indexes of {}", dir.display());
            Ok(true)
        }
        Command::Backup { dir, target } => {
            let db = read_only(&dir)?;
            db.backup_into(&target, elmdb::BackupInto::Backups)?;
            db.close()?;
            println!("{} copied to {}", dir.display(), target.display());
            Ok(true)
        }
        Command::Dump { dir, target } => {
            let db = read_only(&dir)?;
            let report = elmdb::dump::dump(&db, &target)?;
            db.close()?;
            println!("{}", serde_json::to_string_pretty(&report).expect("report serializes"));
            Ok(true)
        }
        Command::Load { dump, target } => {
            let report = elmdb::dump::load(&dump, &target)?;
            println!("{}", serde_json::to_string_pretty(&report).expect("report serializes"));
            Ok(true)
        }
        Command::AcknowledgeRestore { dir } => {
            let db = existing(&dir)?;
            db.acknowledge_restore()?;
            db.close()?;
            println!("Durability sidecar synced for {}", dir.display());
            Ok(true)
        }
        Command::Purge { dir, type_name } => {
            let db = existing(&dir)?;
            let count = db.purge_archived(&type_name)?;
            db.close()?;
            println!("Purged {count} archived '{type_name}' entities");
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
