use std::collections::BTreeMap;
use std::process::Command;

use elmdb::protocol::{FieldDefinition, FieldSpec, IndexDefinition, Schema, TypeDefinition};
use elmdb::{BuildPolicy, CanonicalValue, Database, OpenOptions, WireOp};
use tempfile::tempdir;

fn cli() -> Command {
    Command::new(env!("CARGO_BIN_EXE_elmdb"))
}

fn seed_db(dir: &std::path::Path) {
    let db = Database::open(dir, OpenOptions { build_policy: BuildPolicy::EagerInSchemaChange, ..Default::default() })
        .unwrap();
    let mut fields = BTreeMap::new();
    fields.insert("title".into(), FieldDefinition { kind: FieldSpec::String, optional: false, nullable: false });
    let mut indexes = BTreeMap::new();
    indexes.insert("by_title".into(), IndexDefinition::Value { fields: vec!["title".into()], unique: false });
    let mut types = BTreeMap::new();
    types.insert("doc".into(), TypeDefinition { fields, indexes, ..Default::default() });
    db.set_schema(&Schema { dollar_schema: None, types, relations: BTreeMap::new() }).unwrap();
    db.execute_batch(&[WireOp::Create {
        type_name: "doc".into(),
        external_id: Some("d1".into()),
        fields: vec![("title".into(), CanonicalValue::String("Hello CLI".into()))],
    }])
    .unwrap();
    db.flush().unwrap();
    db.close().unwrap();
}

#[test]
fn cli_inspect_check_rebuild_backup_dump_load_and_missing_db() {
    let tmp = tempdir().unwrap();
    let db_dir = tmp.path().join("db");
    seed_db(&db_dir);

    // inspect
    let out = cli().args(["inspect"]).arg(&db_dir).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let stats: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(stats["typeCounts"]["doc"], 1);

    // check
    let out = cli().args(["check"]).arg(&db_dir).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let chk: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(chk["clean"], true);

    // rebuild
    let out = cli().args(["rebuild"]).arg(&db_dir).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));

    // backup
    let backup_dir = tmp.path().join("backup");
    let out = cli().args(["backup"]).arg(&db_dir).arg(&backup_dir).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));

    // dump + load
    let dump_file = tmp.path().join("dump.json");
    let out = cli().args(["dump"]).arg(&db_dir).arg(&dump_file).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));

    let loaded_dir = tmp.path().join("loaded");
    let out = cli().args(["load"]).arg(&dump_file).arg(&loaded_dir).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));

    let out = cli().args(["check"]).arg(&loaded_dir).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));

    // missing dir refuses with exit code 2
    let missing = tmp.path().join("does-not-exist");
    let out = cli().args(["inspect"]).arg(&missing).output().unwrap();
    assert_eq!(out.status.code(), Some(2));
}
