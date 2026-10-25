//! The TypeScript declarations generated from the protocol's Rust types. The Rust types are the definition; nothing
//! here is written by hand.

use std::collections::BTreeSet;
use std::path::Path;

use ts_rs::{Config, TS};

/// Writes every exported type's declaration into `dir`, replacing what is there
pub fn write(dir: &Path) -> Result<(), String> {
    if dir.exists() {
        std::fs::remove_dir_all(dir).map_err(|e| format!("couldn't clear {}: {e}", dir.display()))?;
    }
    std::fs::create_dir_all(dir).map_err(|e| format!("couldn't create {}: {e}", dir.display()))?;
    let config = Config::new()
        .with_out_dir(dir)
        // Every integer the protocol carries is within ±2^53, so a JavaScript number holds it exactly
        .with_large_int("number")
        // ESM resolution needs the extension on a relative import
        .with_import_extension(Some("js"));
    elmdb::ErrorCode::export_all(&config).map_err(|e| e.to_string())?;
    elmdb::schema::Schema::export_all(&config).map_err(|e| e.to_string())?;
    elmdb::schema::SchemaChange::export_all(&config).map_err(|e| e.to_string())?;
    elmdb::Batch::export_all(&config).map_err(|e| e.to_string())?;
    elmdb::BatchResult::export_all(&config).map_err(|e| e.to_string())?;
    elmdb::ChangeSet::export_all(&config).map_err(|e| e.to_string())?;
    elmdb::engine::plan::Reads::export_all(&config).map_err(|e| e.to_string())?;
    // The JSON Schema a schema's `$schema` names, for editors and for checking a schema before opening a database
    let json_schema = schemars::schema_for!(elmdb::schema::Schema);
    let text = serde_json::to_string_pretty(&json_schema).map_err(|e| e.to_string())? + "\n";
    std::fs::write(dir.join("schema-1.json"), text).map_err(|e| e.to_string())?;
    Ok(())
}

/// The files that differ between two generated directories: added, removed or changed
pub fn differences(fresh: &Path, committed: &Path) -> Result<Vec<String>, String> {
    let names = |dir: &Path| -> Result<BTreeSet<String>, String> {
        if !dir.exists() {
            return Ok(BTreeSet::new());
        }
        std::fs::read_dir(dir)
            .map_err(|e| format!("couldn't read {}: {e}", dir.display()))?
            .map(|entry| entry.map(|e| e.file_name().to_string_lossy().into_owned()).map_err(|e| e.to_string()))
            .collect()
    };
    let (fresh_names, committed_names) = (names(fresh)?, names(committed)?);
    let mut differences = Vec::new();
    for name in fresh_names.union(&committed_names) {
        let read = |dir: &Path| std::fs::read(dir.join(name)).ok();
        match (read(fresh), read(committed)) {
            (Some(_), None) => differences.push(format!("{name}: missing")),
            (None, Some(_)) => differences.push(format!("{name}: no longer generated")),
            (Some(a), Some(b)) if a != b => differences.push(format!("{name}: changed")),
            _ => {}
        }
    }
    Ok(differences)
}
