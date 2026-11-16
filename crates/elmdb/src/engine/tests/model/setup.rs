//! The schema the harness declares and the database it opens

use super::*;

/// The rank index the harness declares, over every entity and kind of edge the workload writes
pub(super) const RANKED: RankIndex<'static> =
    RankIndex { name: "modelRank", types: &["Task", "Note"], kinds: &ALL_KINDS, direction: "both", weights: true };

pub(super) const TOLERANCE: f64 = 1e-9;

/// The harness's schema: the engine tests' with `RANKED` declared and change order kept for `Task` and `Note`, which
/// `changed_since` is asked of. With `spares`, it declares the names renames and purges move data to and from: `spare`, a
/// second unindexed field of `Task`, and the relation kind `tags`, which the workload never links. Without them, both are
/// names a removed declaration left, which a purge takes and a rename can't move to. It declares `Memo` with `Note`'s
/// fields and indexes and no change order, which `Note` is renamed to and back. Every database the model compares opens
/// with it, the crash child's included
pub(super) fn harness_schema(spares: bool) -> Json {
    let mut schema = schema_json();
    schema["types"]["Memo"] = schema["types"]["Note"].clone();
    for entity_type in ["Task", "Note"] {
        schema["types"][entity_type]["changeOrder"] = json!(true);
    }
    if spares {
        schema["types"]["Task"]["fields"]["spare"] = json!({ "type": "any", "optional": true });
    } else {
        schema["relations"].as_object_mut().expect("relations").remove("tags");
    }
    schema["ranks"] = json!({ RANKED.name: { "types": RANKED.types, "relations": RANKED.kinds,
                                              "direction": RANKED.direction, "weights": RANKED.weights,
                                              "tolerance": TOLERANCE } });
    schema
}

/// Declares the spare names again where the workload left them removed. `check()` reports data a removed declaration
/// kept as a violation, a field its type doesn't declare (`architecture/engine.md`, `check()`), and declaring them again
/// is what clears it short of a purge
pub(super) fn declare_the_spares(db: &Database) {
    if !db.schema().is_some_and(|schema| schema.relations.contains_key("tags")) {
        db.set_schema(Schema::from_json(&harness_schema(true)).unwrap()).expect("the spare names declared again");
    }
}

/// The harness's schema, its spare names declared, opened over a fresh directory
pub(super) fn open(dir: &tempfile::TempDir) -> Database {
    let schema = Schema::from_json(&harness_schema(true)).unwrap();
    Database::open(dir.path(), OpenOptions { schema: Some(schema), syncs: false, ..Default::default() }).unwrap()
}
