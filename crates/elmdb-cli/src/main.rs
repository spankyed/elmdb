//! The `elmdb` command: inspect, check, back up and upgrade databases. Each opens the database itself, so the app that
//! holds one open must close it first, except for `inspect`, `check` and `backup`, which open it read-only.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use elmdb::{Database, OpenOptions};

#[derive(Parser)]
#[command(name = "elmdb", version, about = "Inspect, check, back up and upgrade elmdb databases")]
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
    /// An apack format 4 database, dumped by tools/lmdb-js-export, as a new elmdb database
    Upgrade {
        dump: PathBuf,
        target: PathBuf,
        /// The schema to hold the new database to, as JSON
        #[arg(long)]
        schema: Option<PathBuf>,
    },
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
        Command::Upgrade { dump, target, schema } => {
            let schema = match schema {
                Some(path) => {
                    let text = std::fs::read_to_string(&path)?;
                    let json: serde_json::Value = serde_json::from_str(&text).map_err(|e| {
                        elmdb::Error::new(elmdb::ErrorCode::SchemaInvalid, format!("{}: {e}", path.display()))
                    })?;
                    Some(elmdb::schema::Schema::from_json(&json)?)
                }
                None => None,
            };
            let report = elmdb::upgrade::upgrade(&dump, &target, schema)?;
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
