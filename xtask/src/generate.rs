//! TypeScript declarations and JSON Schema generated from elmdb v2's authoritative Rust protocol types.
//!
//! Single source of truth: every exported protocol type and `ErrorCode` is generated from `elmdb` here.

use std::collections::BTreeSet;
use std::path::Path;

use ts_rs::{Config, TS};

/// Writes every exported elmdb v2 protocol declaration + `ErrorCode.d.ts` + `schema-2.json` into `dir`.
pub fn write(dir: &Path) -> Result<(), String> {
    if dir.exists() {
        std::fs::remove_dir_all(dir).map_err(|e| format!("couldn't clear {}: {e}", dir.display()))?;
    }
    std::fs::create_dir_all(dir).map_err(|e| format!("couldn't create {}: {e}", dir.display()))?;
    let config = Config::new()
        .with_out_dir(dir)
        // Every protocol integer fits within JS's safe ±2^53 range.
        .with_large_int("number")
        // ESM resolution requires explicit `.js` extension on relative imports.
        .with_import_extension(Some("js"));

    elmdb::protocol::Schema::export_all(&config).map_err(|e| e.to_string())?;
    elmdb::protocol::SchemaChange::export_all(&config).map_err(|e| e.to_string())?;
    elmdb::protocol::OpenReport::export_all(&config).map_err(|e| e.to_string())?;
    elmdb::protocol::BatchResult::export_all(&config).map_err(|e| e.to_string())?;
    elmdb::protocol::CheckReport::export_all(&config).map_err(|e| e.to_string())?;
    elmdb::protocol::IndexStatus::export_all(&config).map_err(|e| e.to_string())?;
    elmdb::protocol::DatabaseStats::export_all(&config).map_err(|e| e.to_string())?;
    elmdb::protocol::DumpReport::export_all(&config).map_err(|e| e.to_string())?;
    elmdb::protocol::WatchTick::export_all(&config).map_err(|e| e.to_string())?;
    elmdb::protocol::Row::export_all(&config).map_err(|e| e.to_string())?;
    elmdb::protocol::RelationRow::export_all(&config).map_err(|e| e.to_string())?;
    elmdb::protocol::QuerySpec::export_all(&config).map_err(|e| e.to_string())?;
    elmdb::protocol::QueryAnswer::export_all(&config).map_err(|e| e.to_string())?;

    declarations(dir)?;

    // Single source of truth for `ErrorCode` (generated directly from `elmdb_wire::ErrorCode::ALL`).
    let error_code_dts = format!(
        "// This file was generated from elmdb_wire::ErrorCode. Do not edit manually.\n\n{}\n",
        elmdb::ErrorCode::ts_type_definition()
    );
    std::fs::write(dir.join("ErrorCode.d.ts"), error_code_dts).map_err(|e| e.to_string())?;

    let text = serde_json::to_string_pretty(&json_schema()?).map_err(|e| e.to_string())? + "\n";
    std::fs::write(dir.join(JSON_SCHEMA), text).map_err(|e| e.to_string())?;
    Ok(())
}

/// Renames each `.ts` file ts-rs emitted to `.d.ts`.
fn declarations(dir: &Path) -> Result<(), String> {
    let entries = std::fs::read_dir(dir).map_err(|e| format!("couldn't read {}: {e}", dir.display()))?;
    for entry in entries {
        let path = entry.map_err(|e| e.to_string())?.path();
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        if let Some(stem) = name.strip_suffix(".ts").filter(|stem| !stem.ends_with(".d")) {
            let declaration = dir.join(format!("{stem}.d.ts"));
            std::fs::rename(&path, &declaration)
                .map_err(|e| format!("couldn't rename {} to {}: {e}", path.display(), declaration.display()))?;
        }
    }
    Ok(())
}

/// JSON Schema file name for elmdb v2 (`SCHEMA_LANGUAGE = https://elmdb.dev/schema/2.json`).
const JSON_SCHEMA: &str = "schema-2.json";

fn json_schema() -> Result<serde_json::Value, String> {
    let generated = serde_json::to_value(schemars::schema_for!(elmdb::protocol::Schema)).map_err(|e| e.to_string())?;
    let serde_json::Value::Object(generated) = generated else {
        return Err("the schema's JSON Schema isn't an object".to_owned());
    };
    let mut with_id = serde_json::Map::new();
    for (key, value) in generated {
        let dialect = key == "$schema";
        with_id.insert(key, value);
        if dialect {
            with_id.insert("$id".to_owned(), elmdb::protocol::SCHEMA_LANGUAGE.into());
        }
    }
    if !with_id.contains_key("$id") {
        return Err("the schema's JSON Schema names no dialect to write its $id after".to_owned());
    }
    Ok(serde_json::Value::Object(with_id))
}

/// The files that differ between two generated directories: added, removed or changed.
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
        assert_eq!(schema["$id"], elmdb::protocol::SCHEMA_LANGUAGE);
        assert_eq!(schema["$schema"], "https://json-schema.org/draft/2020-12/schema");
        assert_eq!(differences(fresh.path(), committed.path()).unwrap(), Vec::<String>::new());

        let moved = text.replace(elmdb::protocol::SCHEMA_LANGUAGE, "https://elmdb.dev/schema/99.json");
        assert_ne!(moved, text);
        std::fs::write(committed.path().join(JSON_SCHEMA), moved).unwrap();
        assert_eq!(differences(fresh.path(), committed.path()).unwrap(), [format!("{JSON_SCHEMA}: changed")]);
    }

    #[test]
    fn the_protocol_is_written_as_declarations_and_a_committed_ts_file_differs() {
        let (fresh, committed) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        write(fresh.path()).unwrap();
        write(committed.path()).unwrap();
        let mut names: Vec<String> = std::fs::read_dir(fresh.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert!(names.iter().any(|name| name == "Schema.d.ts"), "{names:?}");
        assert!(names.iter().any(|name| name == "ErrorCode.d.ts"), "{names:?}");
        assert!(names.iter().any(|name| name == "QuerySpec.d.ts"), "{names:?}");
        let sources: Vec<&String> =
            names.iter().filter(|name| *name != JSON_SCHEMA && !name.ends_with(".d.ts")).collect();
        assert_eq!(sources, Vec::<&String>::new());

        std::fs::rename(committed.path().join("Schema.d.ts"), committed.path().join("Schema.ts")).unwrap();
        assert_eq!(
            differences(fresh.path(), committed.path()).unwrap(),
            ["Schema.d.ts: missing", "Schema.ts: no longer generated"]
        );
    }
}
