use serde_json::{Value, json};

use super::*;

/// `docs/schema.md`'s complete example
fn example() -> Value {
    json!({
      "$schema": "https://elmdb.dev/schema/1.json",
      "collation": "root",
      "roles": { "assistant_birth": {} },
      "relations": {
        "contains":   { "cardinality": "one-to-many" },
        "parent_of":  { "cardinality": "one-to-many", "acyclic": true },
        "references": { "cardinality": "many-to-many", "unique": true,
                        "endpoints": [{ "source": "Note", "target": "Note" }] }
      },
      "types": {
        "Thread": {
          "declaredBy": "default-setup",
          "shortCode": { "prefix": "THR" },
          "fields": {
            "topic":                { "type": "string" },
            "pinned":               { "type": "boolean", "optional": true },
            "lastMessageTimestamp": { "type": "number", "optional": true },
            "tags":                 { "type": "list", "of": { "type": "string" }, "optional": true },
            "scratchpad":           { "type": "object", "optional": true }
          },
          "indexes": { "pinned": { "kind": "value", "fields": ["pinned"] } }
        },
        "Message": {
          "declaredBy": "default-setup",
          "fields": {
            "text":      { "type": "string" },
            "sender":    { "type": "enum", "values": ["user", "assistant", "system", "marker"] },
            "timestamp": { "type": "number" },
            "status":    { "type": "enum", "values": ["queued", "cancelled"], "optional": true, "nullable": true },
            "blocks":    { "type": "any", "optional": true }
          },
          "indexes": {
            "senderByTime": { "kind": "value", "fields": ["sender", "timestamp"] },
            "text":         { "kind": "text", "fields": { "text": { "weight": 1 } } },
            "meaning":      { "kind": "vector", "from": ["text"], "chunk": "paragraph",
                              "dims": 384, "metric": "cosine", "precision": "f16", "model": "bge-small-en-v1.5" }
          }
        },
        "Note": { "fields": { "title": { "type": "string" } } },
        "SearchIndex": { "fields": {} },
        "IndexedDoc": {
          "declaredBy": "default-setup",
          "fields": {
            "indexId":  { "type": "ref", "to": ["SearchIndex"] },
            "chunkKey": { "type": "string" }
          },
          "indexes": { "chunkKey": { "kind": "value", "fields": ["chunkKey"], "unique": true } }
        }
      }
    })
}

/// The problems a schema has, as `path: message` lines
fn problems(json: Value) -> Vec<String> {
    match parse(&json) {
        Ok(_) => Vec::new(),
        Err(problems) => problems.iter().map(ToString::to_string).collect(),
    }
}

/// The example with `edit` applied
fn edited(edit: impl FnOnce(&mut Value)) -> Value {
    let mut json = example();
    edit(&mut json);
    json
}

#[test]
fn reads_the_documented_example() {
    let schema = parse(&example()).unwrap();
    assert_eq!(schema.types.len(), 5);
    assert_eq!(schema.types["Thread"].short_code.as_ref().unwrap().prefix, "THR");
    assert!(schema.relations["parent_of"].acyclic);
    assert_eq!(
        schema.types["Message"].fields["status"],
        FieldDef {
            value: ValueType::Enum { values: vec!["queued".into(), "cancelled".into()] },
            optional: true,
            nullable: true,
        }
    );
}

#[test]
fn reproduces_the_documented_messages() {
    let json = edited(|j| {
        j["types"]["Message"]["indexes"]["text"]["fields"]["body"] = json!({ "weight": 1 });
        j["relations"]["parent_of"]["endpoints"] =
            json!([{ "source": "Note", "target": "Note" }, { "source": "Note", "target": "Documnet" }]);
    });
    assert_eq!(
        problems(json),
        [
            "relations.parent_of.endpoints[1].target: no such type \"Documnet\"",
            "types.Message.indexes.text.fields.body: no such field on Message (declared by default-setup)",
        ]
    );
}

#[test]
fn lists_every_problem_and_says_so_in_the_error() {
    let json = edited(|j| {
        j["types"]["Thread"]["fields"]["topic"]["typ"] = json!("string");
        j["types"]["Thread"]["fields"]["Pinned"] = json!({ "type": "boolean" });
        j["relations"]["Contains"] = json!({ "cardinality": "lots" });
    });
    let error = Schema::from_json(&json).unwrap_err();
    assert_eq!(error.code, crate::ErrorCode::SchemaInvalid);
    assert_eq!(
        error.message,
        "The schema has 2 problems:\n\
        relations.Contains.cardinality: \"lots\" isn't one of \"one-to-one\", \"one-to-many\", \"many-to-one\", \"many-to-many\"\n\
        types.Thread.fields.topic.typ: unknown key"
    );
    // The structural problems stop the semantic pass: fixed, the names are reported
    let json = edited(|j| {
        j["types"]["Thread"]["fields"]["Pinned"] = json!({ "type": "boolean" });
        j["relations"]["Contains"] = json!({ "cardinality": "one-to-many" });
    });
    assert_eq!(
        problems(json),
        [
            "relations.Contains: a relation kind's name is snake_case, which \"Contains\" isn't",
            "types.Thread.fields.Pinned: a field's name is lowerCamelCase, which \"Pinned\" isn't (declared by default-setup)",
        ]
    );
}

#[test]
fn refuses_unknown_keys_at_every_level() {
    assert_eq!(problems(edited(|j| j["extra"] = json!(1))), ["extra: unknown key"]);
    let json = edited(|j| j["roles"]["assistant_birth"]["exclusive"] = json!(true));
    assert_eq!(problems(json), ["roles.assistant_birth.exclusive: unknown key"]);
    let json = edited(|j| j["types"]["Message"]["indexes"]["text"]["fields"]["text"]["boost"] = json!(2));
    assert_eq!(problems(json), ["types.Message.indexes.text.fields.text.boost: unknown key"]);
    let json = edited(|j| j["types"]["Thread"]["fields"]["tags"]["of"]["optional"] = json!(true));
    assert_eq!(problems(json), ["types.Thread.fields.tags.of.optional: unknown key"]);
}

#[test]
fn refuses_the_wrong_language_and_reserved_names() {
    assert_eq!(
        problems(edited(|j| j["$schema"] = json!("https://elmdb.dev/schema/2.json"))),
        [
            "$schema: \"https://elmdb.dev/schema/2.json\" isn't a schema language this version reads (https://elmdb.dev/schema/1.json)",
        ]
    );
    assert_eq!(
        problems(edited(|j| j["types"]["Note"]["fields"]["createdAt"] = json!({ "type": "number" }))),
        ["types.Note.fields.createdAt: \"createdAt\" is a field the engine writes itself",]
    );
    assert_eq!(
        problems(edited(|j| j["types"]["Relation"] = json!({ "fields": {} }))),
        ["types.Relation: \"Relation\" is a name the engine's own ids use",]
    );
    assert_eq!(
        problems(edited(|j| j["types"]["message_part"] = json!({ "fields": {} }))),
        ["types.message_part: a type's name is PascalCase, which \"message_part\" isn't",]
    );
}

#[test]
fn refuses_short_code_prefixes_that_are_malformed_or_taken() {
    assert_eq!(
        problems(edited(|j| j["types"]["Note"]["shortCode"] = json!({ "prefix": "THR" }))),
        ["types.Thread.shortCode.prefix: \"THR\" is already Note's short code prefix (declared by default-setup)",]
    );
    assert_eq!(
        problems(edited(|j| j["types"]["Note"]["shortCode"] = json!({ "prefix": "no" }))),
        [
            "types.Note.shortCode.prefix: \"no\" isn't a short code prefix: up to 8 capital letters and digits, starting with a letter",
        ]
    );
}

#[test]
fn checks_field_types() {
    assert_eq!(
        problems(edited(|j| j["types"]["Note"]["fields"]["mood"] = json!({ "type": "enum", "values": [] }))),
        ["types.Note.fields.mood.values: lists no value",]
    );
    assert_eq!(
        problems(edited(|j| j["types"]["Note"]["fields"]["kind"] = json!({ "type": "enum", "values": ["a", "a"] }))),
        ["types.Note.fields.kind.values: lists \"a\" twice",]
    );
    assert_eq!(
        problems(edited(|j| j["types"]["Note"]["fields"]["doc"] = json!({ "type": "ref", "to": ["Doc"] }))),
        ["types.Note.fields.doc.to[0]: no such type \"Doc\"",]
    );
    assert_eq!(
        problems(edited(|j| j["types"]["Note"]["fields"]["n"] = json!({ "type": "float" }))),
        [
            "types.Note.fields.n.type: \"float\" isn't a field type (string, number, integer, boolean, bytes, enum, ref, list, object, any)",
        ]
    );
    assert_eq!(
        problems(edited(|j| j["types"]["Note"]["fields"]["n"] = json!({ "type": "number", "optional": "yes" }))),
        ["types.Note.fields.n.optional: must be true or false, not a string",]
    );
}

#[test]
fn checks_what_each_index_covers() {
    let cases = [
        (
            json!({ "kind": "value", "fields": [] }),
            "types.Message.indexes.x.fields: names no field (declared by default-setup)",
        ),
        (
            json!({ "kind": "value", "fields": ["scratch"] }),
            "types.Message.indexes.x.fields[0]: a value index can't hold a object field (\"scratch\") (declared by default-setup)",
        ),
        (
            json!({ "kind": "value", "fields": ["text", "text"] }),
            "types.Message.indexes.x.fields[1]: names \"text\" twice (declared by default-setup)",
        ),
        (
            json!({ "kind": "text", "fields": { "timestamp": {} } }),
            "types.Message.indexes.x.fields.timestamp: a text index reads string and any fields, and \"timestamp\" is a number field (declared by default-setup)",
        ),
        (
            json!({ "kind": "text", "fields": { "text": { "weight": 0 } } }),
            "types.Message.indexes.x.fields.text.weight: must be a positive number (declared by default-setup)",
        ),
        (
            json!({ "kind": "text", "fields": { "text": {} }, "analyzer": "exact", "stem": "en" }),
            "types.Message.indexes.x.stem: only the standard analyzer stems (declared by default-setup)",
        ),
        (
            json!({ "kind": "vector", "from": ["text"], "dims": 0, "model": "m" }),
            "types.Message.indexes.x.dims: must be a whole number from 1 to 4096, not 0",
        ),
        (
            json!({ "kind": "vector", "from": ["sender"], "dims": 8, "model": "m" }),
            "types.Message.indexes.x.from[0]: a vector index embeds string fields, and \"sender\" is a enum field (declared by default-setup)",
        ),
        (
            json!({ "kind": "vector", "from": ["text"], "dims": 8, "model": " " }),
            "types.Message.indexes.x.model: names no model (declared by default-setup)",
        ),
        (
            json!({ "kind": "graph" }),
            "types.Message.indexes.x.kind: \"graph\" isn't an index kind (value, text, vector)",
        ),
    ];
    for (index, expected) in cases {
        let json = edited(|j| {
            j["types"]["Message"]["fields"]["scratch"] = json!({ "type": "object", "optional": true });
            j["types"]["Message"]["indexes"]["x"] = index;
        });
        assert_eq!(problems(json), [expected]);
    }
    // The engine's own times are indexable
    let json = edited(|j| {
        j["types"]["Message"]["indexes"]["byCreation"] = json!({ "kind": "value", "fields": ["createdAt"] })
    });
    assert_eq!(problems(json), Vec::<String>::new());
}

#[test]
fn the_canonical_form_writes_every_default_and_sorts_keys() {
    let schema = parse(
        &json!({ "$schema": SCHEMA_LANGUAGE, "types": { "Note": { "fields": { "title": { "type": "string" } } } } }),
    )
    .unwrap();
    assert_eq!(
        schema.canonical(),
        r#"{"$schema":"https://elmdb.dev/schema/1.json","collation":"root","relations":{},"roles":{},"types":{"Note":{"additionalFields":"refuse","declaredBy":null,"fields":{"title":{"nullable":false,"optional":false,"type":"string"}},"indexes":{},"shortCode":null}}}"#
    );
    assert_eq!(Schema::from_canonical(&schema.canonical()).unwrap(), schema);
}

#[test]
fn the_parser_and_the_stored_form_take_the_same_defaults() {
    // serde's defaults read a stored schema, and the parser's read a given one: a schema leaving out every default
    // reads the same through both
    let sparse = example();
    assert_eq!(serde_json::from_value::<Schema>(sparse.clone()).unwrap(), parse(&sparse).unwrap());
}

#[test]
fn schemas_that_mean_the_same_thing_hash_alike() {
    let a = parse(&example()).unwrap();
    // Every default written out, and the document's keys in reverse order
    let b = parse(&serde_json::from_str::<Value>(&a.canonical()).unwrap()).unwrap();
    let reversed: Value = {
        fn reverse(v: Value) -> Value {
            match v {
                Value::Object(m) => Value::Object(m.into_iter().rev().map(|(k, v)| (k, reverse(v))).collect()),
                Value::Array(a) => Value::Array(a.into_iter().map(reverse).collect()),
                v => v,
            }
        }
        reverse(example())
    };
    let c = parse(&reversed).unwrap();
    assert_eq!(a.hash(), b.hash());
    assert_eq!(a.hash(), c.hash());
    let d = parse(&edited(|j| j["types"]["Note"]["fields"]["title"]["optional"] = json!(true))).unwrap();
    assert_ne!(a.hash(), d.hash());
}

mod changes {
    use super::*;

    fn changes(edit: impl FnOnce(&mut Value)) -> Vec<(String, Change, Effect)> {
        let stored = parse(&example()).unwrap();
        let given = parse(&edited(edit)).unwrap();
        diff(&stored, &given).into_iter().map(|c| (c.path, c.change, c.effect)).collect()
    }

    fn one(path: &str, change: Change, effect: Effect) -> Vec<(String, Change, Effect)> {
        vec![(path.to_owned(), change, effect)]
    }

    #[test]
    fn an_unchanged_schema_changes_nothing() {
        assert_eq!(changes(|_| {}), []);
    }

    #[test]
    fn additions_are_applied_and_new_indexes_build() {
        assert_eq!(changes(|j| j["roles"]["pinned"] = json!({})), one("roles.pinned", Change::Added, Effect::Applied));
        assert_eq!(
            changes(|j| j["relations"]["blocks"] = json!({ "cardinality": "many-to-many" })),
            one("relations.blocks", Change::Added, Effect::Applied)
        );
        assert_eq!(
            changes(|j| j["types"]["Note"]["fields"]["body"] = json!({ "type": "string", "optional": true })),
            one("types.Note.fields.body", Change::Added, Effect::Applied)
        );
        assert_eq!(
            changes(|j| j["types"]["Note"]["indexes"]["title"] = json!({ "kind": "value", "fields": ["title"] })),
            one("types.Note.indexes.title", Change::Added, Effect::Builds)
        );
        assert_eq!(
            changes(
                |j| j["types"]["Tab"] = json!({ "fields": { "url": { "type": "string" } }, "indexes": { "url": { "kind": "value", "fields": ["url"] } } })
            ),
            vec![
                ("types.Tab".to_owned(), Change::Added, Effect::Applied),
                ("types.Tab.indexes.url".to_owned(), Change::Added, Effect::Builds),
            ]
        );
    }

    #[test]
    fn a_changed_index_rebuilds_and_a_removed_one_drops() {
        assert_eq!(
            changes(|j| j["types"]["Message"]["indexes"]["text"]["fields"]["text"]["weight"] = json!(2)),
            one("types.Message.indexes.text", Change::Changed, Effect::Rebuilds)
        );
        assert_eq!(
            changes(|j| {
                j["types"]["Thread"]["indexes"].as_object_mut().unwrap().remove("pinned");
            }),
            one("types.Thread.indexes.pinned", Change::Removed, Effect::Drops)
        );
    }

    #[test]
    fn a_removed_declaration_keeps_its_data() {
        assert_eq!(
            changes(|j| {
                j["types"]["Thread"]["fields"].as_object_mut().unwrap().remove("tags");
            }),
            one("types.Thread.fields.tags", Change::Removed, Effect::KeepsData)
        );
        assert_eq!(
            changes(|j| {
                j["roles"].as_object_mut().unwrap().remove("assistant_birth");
            }),
            one("roles.assistant_birth", Change::Removed, Effect::KeepsData)
        );
        assert_eq!(
            changes(|j| {
                j["types"].as_object_mut().unwrap().remove("Thread");
            }),
            vec![
                ("types.Thread".to_owned(), Change::Removed, Effect::KeepsData),
                ("types.Thread.indexes.pinned".to_owned(), Change::Removed, Effect::Drops),
            ]
        );
    }

    #[test]
    fn a_tightened_rule_is_enforced_and_a_loosened_one_applied() {
        let field = "types.Thread.fields.pinned";
        assert_eq!(
            changes(|j| j["types"]["Thread"]["fields"]["pinned"]["optional"] = json!(false)),
            one(field, Change::Changed, Effect::Enforced)
        );
        assert_eq!(
            changes(|j| j["types"]["Message"]["fields"]["status"]["nullable"] = json!(false)),
            one("types.Message.fields.status", Change::Changed, Effect::Enforced)
        );
        assert_eq!(
            changes(|j| j["types"]["Message"]["fields"]["sender"]["values"] = json!(["user"])),
            one("types.Message.fields.sender", Change::Changed, Effect::Enforced)
        );
        assert_eq!(
            changes(|j| j["types"]["Message"]["fields"]["sender"]["values"] =
                json!(["user", "assistant", "system", "marker", "tool"])),
            one("types.Message.fields.sender", Change::Changed, Effect::Applied)
        );
        assert_eq!(
            changes(|j| j["types"]["Message"]["fields"]["sender"] = json!({ "type": "string" })),
            one("types.Message.fields.sender", Change::Changed, Effect::Applied)
        );
        assert_eq!(
            changes(|j| j["types"]["Thread"]["fields"]["topic"] = json!({ "type": "any" })),
            one("types.Thread.fields.topic", Change::Changed, Effect::Applied)
        );
        assert_eq!(
            changes(|j| j["relations"]["contains"]["cardinality"] = json!("one-to-one")),
            one("relations.contains", Change::Changed, Effect::Enforced)
        );
        assert_eq!(
            changes(|j| j["relations"]["contains"]["cardinality"] = json!("many-to-many")),
            one("relations.contains", Change::Changed, Effect::Applied)
        );
        assert_eq!(
            changes(|j| j["relations"]["contains"]["acyclic"] = json!(true)),
            one("relations.contains", Change::Changed, Effect::Enforced)
        );
        assert_eq!(
            changes(|j| j["relations"]["contains"]["endpoints"] = json!([{ "source": "Thread", "target": "Message" }])),
            one("relations.contains", Change::Changed, Effect::Enforced)
        );
        assert_eq!(
            changes(|j| {
                j["relations"]["references"].as_object_mut().unwrap().remove("endpoints");
            }),
            one("relations.references", Change::Changed, Effect::Applied)
        );
        assert_eq!(
            changes(|j| j["types"]["Note"]["additionalFields"] = json!("allow")),
            one("types.Note.additionalFields", Change::Changed, Effect::Applied)
        );
        assert_eq!(
            changes(|j| j["types"]["Thread"]["indexes"]["pinned"]["unique"] = json!(true)),
            vec![
                ("types.Thread.indexes.pinned".to_owned(), Change::Changed, Effect::Rebuilds),
                ("types.Thread.indexes.pinned.unique".to_owned(), Change::Changed, Effect::Enforced),
            ]
        );
    }

    #[test]
    fn a_short_code_prefix_never_changes() {
        assert_eq!(
            changes(|j| j["types"]["Thread"]["shortCode"]["prefix"] = json!("THD")),
            one("types.Thread.shortCode.prefix", Change::Changed, Effect::Refused)
        );
        assert_eq!(
            changes(|j| {
                j["types"]["Thread"].as_object_mut().unwrap().remove("shortCode");
            }),
            one("types.Thread.shortCode", Change::Removed, Effect::Refused)
        );
        assert_eq!(
            changes(|j| j["types"]["Note"]["shortCode"] = json!({ "prefix": "NTE" })),
            one("types.Note.shortCode", Change::Added, Effect::Applied)
        );
    }

    #[test]
    fn a_new_collation_is_applied_since_indexes_hold_values_as_written() {
        assert_eq!(changes(|j| j["collation"] = json!("en")), one("collation", Change::Changed, Effect::Applied));
    }
}
