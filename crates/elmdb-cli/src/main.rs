//! The `elmdb` command: inspect, check, back up, dump and load databases. Each opens the database itself, so the app
//! that holds one open must close it first, except for `inspect`, `check`, `backup` and `dump`, which open it read-only.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use elmdb::{Database, OpenOptions};

#[derive(Parser)]
#[command(name = "elmdb", version, about = "Inspect, check, back up, dump and load elmdb databases")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// What a database holds: entities per type and field, relations per kind, entries per index, its sizes
    Inspect { dir: PathBuf },
    /// Compares derived data with primary data and every row with the schema's rules; exits 1 when it finds anything
    Check { dir: PathBuf },
    /// A compacted copy of a database into a new directory
    Backup { dir: PathBuf, target: PathBuf },
    /// A database's schema and primary data into a new file, which `load` makes a database of again
    Dump { dir: PathBuf, target: PathBuf },
    /// A dump into a new database, held to the dump's schema, its derived data built
    Load { dump: PathBuf, target: PathBuf },
}

fn read_only(dir: &PathBuf) -> elmdb::Result<Database> {
    Database::open(dir, OpenOptions { read_only: true, ..Default::default() })
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
        Command::Backup { dir, target } => {
            let db = Database::open(&dir, OpenOptions { read_only: true, ..Default::default() })?;
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
