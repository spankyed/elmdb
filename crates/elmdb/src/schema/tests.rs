use serde_json::{Value, json};

use super::*;

/// `docs/architecture/schema.md`'s complete example
fn example() -> Value {
    json!({
      "$schema": "https://elmdb.dev/schema/1.json",
      "collation": "root",
      "roles": { "assistant_birth": {} },
      "relations": {
        "contains":   { "cardinality": "one-to-many" },
        "parent_of":  { "cardinality": "one-to-many", "acyclic": true },
        "references": { "cardinality": "many-to-many", "key": ["via"],
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
                              "dims": 384, "metric": "cosine", "precision": "f32", "model": "bge-small-en-v1.5" }
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
fn a_relation_key_names_info_fields_once_on_info_that_is_a_map() {
    assert_eq!(parse(&example()).unwrap().relations["references"].key, ["via"]);
    assert!(parse(&example()).unwrap().relations["contains"].key.is_empty(), "no key is one edge a pair");
    assert_eq!(
        problems(edited(|j| j["relations"]["references"]["key"] = json!(["via", "via"]))),
        ["relations.references.key[1]: \"via\" is named twice"]
    );
    assert_eq!(
        problems(edited(|j| j["relations"]["references"]["key"] = json!(["Via"]))),
        ["relations.references.key[0]: a key field's name is lowerCamelCase, which \"Via\" isn't"]
    );
    assert_eq!(
        problems(edited(|j| j["relations"]["references"]["info"] = json!({ "type": "integer" }))),
        ["relations.references.key: names info fields, so the kind's info is an object or any"]
    );
    let unique = problems(edited(|j| j["relations"]["contains"]["unique"] = json!(true)));
    assert_eq!(unique.len(), 1, "{unique:?}");
    assert!(unique[0].starts_with("relations.contains.unique"), "{unique:?}");
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
            "types.Message.indexes.x.from[0]: a vector index embeds string and any fields, and \"sender\" is a enum field (declared by default-setup)",
        ),
        (
            json!({ "kind": "vector", "from": ["text"], "dims": 8, "model": "m", "metric": "dot" }),
            "types.Message.indexes.x.metric: is cosine or l2: a normalized embedding ranks by dot product as it does by cosine (declared by default-setup)",
        ),
        (
            json!({ "kind": "vector", "from": ["text"], "dims": 8, "model": "m", "precision": "i8" }),
            "types.Message.indexes.x.precision: is f32, the one precision the graphs store (declared by default-setup)",
        ),
        (
            json!({ "kind": "vector", "from": ["text"], "dims": 8, "model": " " }),
            "types.Message.indexes.x.model: names no model (declared by default-setup)",
        ),
        (
            json!({ "kind": "trigram", "fields": [] }),
            "types.Message.indexes.x.fields: names no field (declared by default-setup)",
        ),
        (
            json!({ "kind": "trigram", "fields": ["nope"] }),
            "types.Message.indexes.x.fields[0]: no such field on Message (declared by default-setup)",
        ),
        (
            json!({ "kind": "trigram", "fields": ["sender"] }),
            "types.Message.indexes.x.fields[0]: a trigram index reads string and any fields, and \"sender\" is a enum field (declared by default-setup)",
        ),
        (
            json!({ "kind": "graph" }),
            "types.Message.indexes.x.kind: \"graph\" isn't an index kind (value, text, trigram, vector)",
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
        r#"{"$schema":"https://elmdb.dev/schema/1.json","collation":"root","ranks":{},"relations":{},"roles":{},"types":{"Note":{"additionalFields":"refuse","declaredBy":null,"fields":{"title":{"nullable":false,"optional":false,"type":"string"}},"indexes":{},"shortCode":null}}}"#
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
        diff(&stored, &given, &Held::default()).into_iter().map(|c| (c.path, c.change, c.effect)).collect()
    }

    fn changes_between(
        before: impl FnOnce(&mut Value),
        after: impl FnOnce(&mut Value),
    ) -> Vec<(String, Change, Effect)> {
        let stored = parse(&edited(before)).unwrap();
        let given = parse(&edited(after)).unwrap();
        diff(&stored, &given, &Held::default()).into_iter().map(|c| (c.path, c.change, c.effect)).collect()
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
            changes_between(
                |j| j["relations"]["contains"]["cardinality"] = json!("one-to-one"),
                |j| j["relations"]["contains"]["cardinality"] = json!("one-to-many"),
            ),
            one("relations.contains", Change::Changed, Effect::Applied)
        );
        assert_eq!(
            changes(|j| j["types"]["Thread"]["fields"]["topic"] = json!({ "type": "integer" })),
            one("types.Thread.fields.topic", Change::Changed, Effect::Enforced)
        );
        assert_eq!(
            changes(|j| j["types"]["Thread"]["fields"]["topic"]["optional"] = json!(true)),
            one("types.Thread.fields.topic", Change::Changed, Effect::Applied)
        );
        assert_eq!(
            changes(|j| j["relations"]["contains"]["acyclic"] = json!(true)),
            one("relations.contains", Change::Changed, Effect::Enforced)
        );
        // A key that drops a field can make one identity of edges it told apart; one that adds a field can't
        assert_eq!(
            changes(|j| j["relations"]["references"]["key"] = json!([])),
            one("relations.references", Change::Changed, Effect::Enforced)
        );
        assert_eq!(
            changes(|j| j["relations"]["references"]["key"] = json!(["via", "at"])),
            one("relations.references", Change::Changed, Effect::Applied)
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
    fn a_field_added_to_a_type_allowing_any_holds_its_rows_to_its_type() {
        let mut stored = example();
        stored["types"]["Note"]["additionalFields"] = json!("allow");
        let mut given = stored.clone();
        given["types"]["Note"]["fields"]["loose"] = json!({ "type": "any", "optional": true, "nullable": true });
        given["types"]["Note"]["fields"]["strict"] = json!({ "type": "string", "optional": true });
        let (stored, given) = (parse(&stored).unwrap(), parse(&given).unwrap());
        let held = Held { types: ["Note".to_owned()].into(), ..Held::default() };
        let effects: Vec<(String, Effect)> =
            diff(&stored, &given, &held).into_iter().map(|c| (c.path, c.effect)).collect();
        assert_eq!(
            effects,
            [
                ("types.Note.fields.loose".to_owned(), Effect::Applied),
                ("types.Note.fields.strict".to_owned(), Effect::Enforced)
            ]
        );
    }

    #[test]
    fn a_new_collation_is_applied_since_indexes_hold_values_as_written() {
        assert_eq!(changes(|j| j["collation"] = json!("en")), one("collation", Change::Changed, Effect::Applied));
    }
}

#[test]
fn a_collation_is_a_locale_the_collator_reads() {
    // Each part has a subtag's shape, and "1a" is no subtag a locale has
    assert_eq!(problems(edited(|j| j["collation"] = json!("en-1a"))), ["collation: \"en-1a\" isn't a locale"]);
    assert_eq!(problems(edited(|j| j["collation"] = json!("de-AT"))), Vec::<String>::new());
    assert_eq!(problems(edited(|j| j["collation"] = json!("root"))), Vec::<String>::new());
}

#[test]
fn an_index_may_not_take_the_name_of_the_engine_s_own() {
    for name in ["role", "shortCode"] {
        let mut schema = example();
        schema["types"]["Thread"]["indexes"][name] = json!({ "kind": "value", "fields": ["topic"] });
        let problem = Schema::from_json(&schema).unwrap_err();
        assert_eq!(problem.code, crate::error::ErrorCode::SchemaInvalid);
        assert!(problem.message.contains("the engine's own index"), "{}", problem.message);
    }
}

#[test]
fn the_canonical_form_is_the_same_whatever_order_a_set_is_listed_in() {
    let mut a = example();
    let mut b = example();
    a["types"]["Message"]["fields"]["sender"]["values"] = json!(["user", "assistant", "system", "marker"]);
    b["types"]["Message"]["fields"]["sender"]["values"] = json!(["marker", "system", "assistant", "user"]);
    a["relations"]["references"]["endpoints"] =
        json!([{ "source": "Note", "target": "Note" }, { "source": "Thread", "target": "Note" }]);
    b["relations"]["references"]["endpoints"] =
        json!([{ "source": "Thread", "target": "Note" }, { "source": "Note", "target": "Note" }]);
    let (a, b) = (Schema::from_json(&a).unwrap(), Schema::from_json(&b).unwrap());
    assert_eq!(a.hash(), b.hash());
    // An index's fields are an order, and stay one
    let mut c = example();
    c["types"]["Message"]["indexes"]["senderByTime"]["fields"] = json!(["timestamp", "sender"]);
    assert_ne!(Schema::from_json(&c).unwrap().hash(), a.hash());
}

mod ranks {
    use super::*;

    fn with_rank(rank: Value) -> Value {
        edited(|j| j["ranks"] = json!({ "noteRank": rank }))
    }

    #[test]
    fn a_rank_index_takes_its_defaults_whether_parsed_or_stored() {
        let parsed = parse(&with_rank(json!({ "types": ["Note"], "relations": ["references"] }))).unwrap();
        let rank = &parsed.ranks["noteRank"];
        assert_eq!(
            (rank.direction, rank.weights, rank.damping, rank.tolerance, rank.declared_by.as_deref()),
            (Direction::Out, true, 0.85, 1e-6, None)
        );
        // A stored schema's defaults are serde's, and read the same
        let stored: RankDef =
            serde_json::from_value(json!({ "types": ["Note"], "relations": ["references"] })).unwrap();
        assert_eq!(&stored, rank);
        let given = parse(&with_rank(json!({ "types": ["Note"], "relations": ["references"], "direction": "both",
            "weights": false, "damping": 0.5, "tolerance": 0.01 })))
        .unwrap();
        let rank = &given.ranks["noteRank"];
        assert_eq!((rank.direction, rank.weights, rank.damping, rank.tolerance), (Direction::Both, false, 0.5, 0.01));
    }

    #[test]
    fn a_rank_index_is_refused_for_each_thing_it_gets_wrong() {
        let refused = |rank: Value| problems(with_rank(rank)).join("\n");
        let ok = json!({ "types": ["Note"], "relations": ["references"] });
        assert_eq!(refused(ok.clone()), "");
        let with = |key: &str, value: Value| {
            let mut rank = ok.clone();
            rank[key] = value;
            refused(rank)
        };
        assert!(with("types", json!([])).contains("names no type"));
        assert!(with("types", json!(["Note", "Note"])).contains("named twice"));
        assert!(with("types", json!(["Nope"])).contains("no such type"));
        assert!(with("relations", json!([])).contains("names no relation kind"));
        assert!(with("relations", json!(["references", "references"])).contains("named twice"));
        assert!(with("relations", json!(["nope"])).contains("no such relation kind"));
        for damping in [0.0, 1.0, -0.5, 1.5] {
            assert!(with("damping", json!(damping)).contains("damping"), "{damping}");
        }
        assert_eq!(with("damping", json!(0.999)), "");
        for tolerance in [0.0, -1.0] {
            assert!(with("tolerance", json!(tolerance)).contains("tolerance"), "{tolerance}");
        }
        assert_eq!(with("tolerance", json!(1e-300)), "");
        let named = |name: &str| problems(edited(|j| j["ranks"] = json!({ name: ok.clone() }))).join("\n");
        assert!(named("NoteRank").contains("lowerCamelCase"));
        // An index of a type already holds the name
        let shared = problems(edited(|j| {
            j["ranks"] = json!({ "pinned": ok.clone() });
        }))
        .join("\n");
        assert!(shared.contains("already an index of Thread"), "{shared}");
    }

    #[test]
    fn a_rank_index_builds_rebuilds_and_drops_with_its_graph() {
        let stored = parse(&with_rank(json!({ "types": ["Note"], "relations": ["references", "contains"] }))).unwrap();
        let diffed = |given: Value| -> Vec<(String, Change, Effect)> {
            let given = parse(&given).unwrap();
            diff(&stored, &given, &Held::default()).into_iter().map(|c| (c.path, c.change, c.effect)).collect()
        };
        let rank = |extra: Value| {
            let mut rank = json!({ "types": ["Note"], "relations": ["references", "contains"] });
            rank.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
            rank
        };
        let at = "ranks.noteRank".to_owned();
        assert_eq!(diffed(with_rank(rank(json!({})))), []);
        // Who declared it holds nothing the index holds
        assert_eq!(diffed(with_rank(rank(json!({ "declaredBy": "a-pack" })))), []);
        assert_eq!(
            diffed(with_rank(rank(json!({ "tolerance": 1e-3 })))),
            [(at.clone(), Change::Changed, Effect::Rebuilds)]
        );
        assert_eq!(diffed(edited(|j| j["ranks"] = json!({}))), [(at.clone(), Change::Removed, Effect::Drops)]);
        let added = parse(&edited(|_| {})).unwrap();
        let built: Vec<_> =
            diff(&added, &stored, &Held::default()).into_iter().map(|c| (c.path, c.change, c.effect)).collect();
        assert!(built.contains(&(at.clone(), Change::Added, Effect::Builds)), "{built:?}");
        // A kind it reads made weighted rebuilds it; one it doesn't read, or read without weights, doesn't
        let weighted = |kind: &str, weights: bool| {
            let mut json = with_rank(rank(json!({ "weights": weights })));
            json["relations"][kind]["weighted"] = json!(true);
            json
        };
        let changed = diffed(weighted("references", true));
        assert!(changed.contains(&(at.clone(), Change::Changed, Effect::Rebuilds)), "{changed:?}");
        let unread = diffed(weighted("parent_of", true));
        assert!(!unread.iter().any(|c| c.0 == at), "{unread:?}");
        let stored_unweighed = parse(&with_rank(rank(json!({ "weights": false })))).unwrap();
        let given = parse(&weighted("references", false)).unwrap();
        let without = diff(&stored_unweighed, &given, &Held::default());
        assert!(!without.iter().any(|c| c.path == at), "{without:?}");
    }
}
