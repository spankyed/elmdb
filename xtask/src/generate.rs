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
    elmdb::OpenReport::export_all(&config).map_err(|e| e.to_string())?;
    elmdb::Batch::export_all(&config).map_err(|e| e.to_string())?;
    elmdb::BatchResult::export_all(&config).map_err(|e| e.to_string())?;
    elmdb::engine::Written::export_all(&config).map_err(|e| e.to_string())?;
    elmdb::ChangeSet::export_all(&config).map_err(|e| e.to_string())?;
    elmdb::Changed::export_all(&config).map_err(|e| e.to_string())?;
    elmdb::engine::plan::Reads::export_all(&config).map_err(|e| e.to_string())?;
    elmdb::engine::answer::Answers::export_all(&config).map_err(|e| e.to_string())?;
    elmdb::engine::maintenance::Load::export_all(&config).map_err(|e| e.to_string())?;
    elmdb::engine::maintenance::Stats::export_all(&config).map_err(|e| e.to_string())?;
    elmdb::engine::derive::CheckReport::export_all(&config).map_err(|e| e.to_string())?;
    elmdb::engine::IndexStatus::export_all(&config).map_err(|e| e.to_string())?;
    let text = serde_json::to_string_pretty(&json_schema()?).map_err(|e| e.to_string())? + "\n";
    std::fs::write(dir.join(JSON_SCHEMA), text).map_err(|e| e.to_string())?;
    Ok(())
}

/// The JSON Schema's file among the generated ones: the 1 is the schema language's version
const JSON_SCHEMA: &str = "schema-1.json";

/// The JSON Schema a schema's `$schema` names, for editors and for checking a schema before opening a database. Its
/// `$id` is that name, `SCHEMA_LANGUAGE`, the URL it is meant to be served at, written after the `$schema` that names
/// its own dialect
fn json_schema() -> Result<serde_json::Value, String> {
    let generated = serde_json::to_value(schemars::schema_for!(elmdb::schema::Schema)).map_err(|e| e.to_string())?;
    let serde_json::Value::Object(generated) = generated else {
        return Err("the schema's JSON Schema isn't an object".to_owned());
    };
    let mut with_id = serde_json::Map::new();
    for (key, value) in generated {
        let dialect = key == "$schema";
        with_id.insert(key, value);
        if dialect {
            with_id.insert("$id".to_owned(), elmdb::schema::SCHEMA_LANGUAGE.into());
        }
    }
    if !with_id.contains_key("$id") {
        return Err("the schema's JSON Schema names no dialect to write its $id after".to_owned());
    }
    Ok(serde_json::Value::Object(with_id))
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

#[cfg(test)]
mod tests {
    use super::{JSON_SCHEMA, differences, write};

    #[test]
    fn the_json_schema_is_identified_by_the_schema_language_and_a_committed_one_that_isnt_differs() {
        let (fresh, committed) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        write(fresh.path()).unwrap();
        write(committed.path()).unwrap();
        let text = std::fs::read_to_string(fresh.path().join(JSON_SCHEMA)).unwrap();
        let schema: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(schema["$id"], elmdb::schema::SCHEMA_LANGUAGE);
        // The dialect's URL stays the JSON Schema's own, and the two are told apart
        assert_eq!(schema["$schema"], "https://json-schema.org/draft/2020-12/schema");
        assert_eq!(differences(fresh.path(), committed.path()).unwrap(), Vec::<String>::new());

        // One byte of the committed `$id` changed is what `check-generated` fails on
        let moved = text.replace(elmdb::schema::SCHEMA_LANGUAGE, "https://elmdb.dev/schema/2.json");
        assert_ne!(moved, text);
        std::fs::write(committed.path().join(JSON_SCHEMA), moved).unwrap();
        assert_eq!(differences(fresh.path(), committed.path()).unwrap(), [format!("{JSON_SCHEMA}: changed")]);
    }
}
