//! Ordered reads, ranges and pages (`docs/architecture/engine.md`, Reads: plans): what a plan reads from a value index
//! that accounts for every entity of its type, and where a page's cursor is sought. `Item.n` is in an index of its own
//! and `Item.m`, which every write here gives the same value, is in none, so a plan over `n` is held to what the same
//! plan answers over `m`, which is sorted, filtered and walked; what a plan visited shows which of the two it did.

use std::collections::BTreeMap;

use rmpv::Value;
use serde_json::{Value as Json, json};

use crate::engine::batch::{Batch, Op};
use crate::engine::keys;
use crate::engine::plan::{Plan, ReadOp, Reads, Scope, Start, Step, Terminal};
use crate::engine::unit::Read;
use crate::error::ErrorCode;
use crate::format::Prefix;
use crate::schema::Schema;
use crate::{Database, OpenOptions};

fn schema_json() -> Json {
    json!({
        "$schema": crate::schema::SCHEMA_LANGUAGE,
        "roles": { "star": {} },
        "types": {
            "Item": {
                "fields": {
                    "n": { "type": "any", "optional": true },
                    "m": { "type": "any", "optional": true },
                    "k": { "type": "number", "optional": true },
                    "tag": { "type": "string", "optional": true },
                    "note": { "type": "string", "optional": true }
                },
                "indexes": {
                    "byN": { "kind": "value", "fields": ["n"] },
                    "byTag": { "kind": "value", "fields": ["tag"] },
                    "made": { "kind": "value", "fields": ["createdAt"] },
                    "written": { "kind": "value", "fields": ["updatedAt"] }
                }
            },
            "Other": {
                "fields": { "n": { "type": "any", "optional": true }, "m": { "type": "any", "optional": true } },
                "indexes": {
                    "byN": { "kind": "value", "fields": ["n"] },
                    "byM": { "kind": "value", "fields": ["m"] }
                }
            }
        }
    })
}

fn open_with(schema: Json) -> (tempfile::TempDir, Database) {
    let dir = tempfile::tempdir().unwrap();
    let schema = Schema::from_json(&schema).unwrap();
    let db =
        Database::open(dir.path(), OpenOptions { schema: Some(schema), syncs: false, ..Default::default() }).unwrap();
    (dir, db)
}

fn open() -> (tempfile::TempDir, Database) {
    open_with(schema_json())
}

fn write(db: &Database, ops: Vec<Op>) {
    db.write(&Batch { ops, now: Some(1000.0) }, false).unwrap();
}

fn op(json: Json) -> Op {
    serde_json::from_value(json).unwrap()
}

/// A new entity whose `n` and `m` hold `value`, or that holds neither, with `more` fields
fn create(entity_type: &str, id: &str, value: Option<Value>, more: &[(&str, Value)]) -> Op {
    let mut fields: BTreeMap<String, Value> = more.iter().map(|(name, v)| ((*name).to_owned(), v.clone())).collect();
    if let Some(value) = value {
        fields.insert("n".into(), value.clone());
        fields.insert("m".into(), value);
    }
    Op::Create {
        entity_type: entity_type.into(),
        id: Some(id.into()),
        fields,
        short_code: None,
        created_at: None,
        updated_at: None,
    }
}

/// `n` and `m` of an entity set to `value`
fn set(id: &str, value: Value) -> Op {
    Op::Set { id: id.into(), fields: [("n".to_owned(), value.clone()), ("m".to_owned(), value)].into(), touch: true }
}

/// Entities of a type, `<type>-00` on, each holding the value at its place in `n` and `m`
fn load(db: &Database, entity_type: &str, values: &[Option<Value>]) -> Vec<String> {
    let ids: Vec<String> = (0..values.len()).map(|at| format!("{entity_type}-{at:02}")).collect();
    write(db, ids.iter().zip(values).map(|(id, value)| create(entity_type, id, value.clone(), &[])).collect());
    ids
}

/// Booleans, numbers that repeat, nulls and entities without the field, in no order
fn mixed() -> Vec<Option<Value>> {
    vec![
        Some(3.into()),
        Some(1.into()),
        Some(Value::Nil),
        None,
        Some(2.5.into()),
        Some(true.into()),
        Some(1.into()),
        Some(false.into()),
        Some((-4).into()),
        Some(0.into()),
        Some(3.into()),
        None,
        Some(10.into()),
        Some(1.0.into()),
        Some(Value::Nil),
        Some((-0.5).into()),
        Some(true.into()),
        Some(3.into()),
    ]
}

/// `mixed`'s entities in `n`'s order, by their places: false, true, the numbers, then those without a value, ties in
/// creation order
const ASCENDING: [usize; 18] = [7, 5, 16, 8, 15, 9, 1, 6, 13, 4, 0, 10, 17, 12, 2, 3, 11, 14];
/// And descending, which reverses the values and not the ties
const DESCENDING: [usize; 18] = [2, 3, 11, 14, 12, 0, 10, 17, 4, 1, 6, 13, 9, 15, 8, 5, 16, 7];

fn read(db: &Database, op: Json) -> crate::Result<Json> {
    let reads = Reads { reads: vec![serde_json::from_value::<ReadOp>(op).unwrap()], track: None };
    let bytes = db.read(&reads)?;
    let mut answers = serde_json::to_value(rmpv::decode::read_value(&mut &bytes[..]).unwrap()).unwrap();
    Ok(answers[0].take())
}

fn plan(scope: &str, types: &[&str], steps: Json) -> Json {
    json!({ "scope": scope, "start": { "from": "types", "types": types }, "steps": steps, "terminal": { "answer": "ids" } })
}

/// The ids a plan answers, and the entities it visited
fn visited(db: &Database, plan: Json) -> (Vec<String>, u64) {
    let answer = read(db, json!({ "read": "query", "visits": true, "plan": plan })).unwrap();
    let ids = answer["answer"]["ids"].as_array().unwrap().iter().map(|id| id.as_str().unwrap().to_owned()).collect();
    (ids, answer["visited"].as_u64().unwrap())
}

fn ids(db: &Database, plan: Json) -> Vec<String> {
    visited(db, plan).0
}

/// What a plan of Items answers reading `n`, and what it visited, once that is what it answers reading `m`
fn same(db: &Database, scope: &str, steps: impl Fn(&str) -> Json) -> (Vec<String>, u64) {
    let (indexed, visits) = visited(db, plan(scope, &["Item"], steps("n")));
    let plain = ids(db, plan(scope, &["Item"], steps("m")));
    assert_eq!(indexed, plain, "{scope}: {}", steps("n"));
    (indexed, visits)
}

fn order(field: &str, descending: bool) -> Json {
    json!({ "step": "orderBy", "field": field, "descending": descending })
}

fn limit(n: usize) -> Json {
    json!({ "step": "limit", "n": n })
}

fn after(id: &str) -> Json {
    json!({ "step": "after", "id": id })
}

/// The ids an index that accounts for every entity files under `prefix`, in place order
fn filed(db: &Database, prefix: Prefix, entity_type: &str, index: &str) -> Vec<String> {
    db.snapshot(|s| {
        let under = keys::index_others(prefix, entity_type, index, &[]);
        Ok(super::filed_under(s, entity_type, &under).into_iter().map(|(_, id)| id).collect())
    })
    .unwrap()
}

fn clean(db: &Database) {
    let report = db.check().unwrap();
    assert!(report.is_clean(), "{report:?}");
}

#[test]
fn an_order_is_read_from_the_index_as_the_sort_has_it() {
    let (_dir, db) = open();
    let all = load(&db, "Item", &mixed());
    for (descending, places) in [(false, ASCENDING), (true, DESCENDING)] {
        let (sorted, visits) = same(&db, "live", |field| json!([order(field, descending)]));
        let expected: Vec<&String> = places.iter().map(|at| &all[*at]).collect();
        assert_eq!(sorted.iter().collect::<Vec<_>>(), expected, "descending {descending}");
        assert_eq!(visits as usize, all.len());
        for take in [1, 2, 3, 4, 5, 9, 14, 15, all.len(), all.len() + 3] {
            let (first, visits) = same(&db, "live", |field| json!([order(field, descending), limit(take)]));
            let kept = take.min(all.len());
            assert_eq!(first, sorted[..kept], "descending {descending}, the first {take}");
            assert_eq!(visits as usize, kept, "descending {descending}: {take} are read for the first {take}");
        }
    }
    // The sort itself reads every entity for its first
    let (_, sorted) = visited(&db, plan("live", &["Item"], json!([order("m", false), limit(1)])));
    assert_eq!(sorted as usize, all.len());
    clean(&db);
}

#[test]
fn a_value_the_index_does_not_order_has_the_order_sorted_until_it_goes() {
    let ext = Value::Ext(-1, 1_700_000_000u32.to_be_bytes().to_vec());
    let unordered: Vec<(&str, Value)> = vec![
        ("a list", Value::Array(vec![1.into()])),
        ("an empty list", Value::Array(vec![])),
        ("a map", Value::Map(vec![("k".into(), 1.into())])),
        ("a timestamp", ext),
        ("bytes", Value::Binary(vec![1, 2])),
    ];
    for (what, value) in unordered {
        let (_dir, db) = open();
        let all = load(&db, "Item", &mixed());
        write(&db, vec![create("Item", "Item-odd", Some(value.clone()), &[])]);
        for descending in [false, true] {
            let (_, visits) = same(&db, "live", |field| json!([order(field, descending), limit(3)]));
            assert_eq!(visits as usize, all.len() + 1, "{what}: every entity is read and sorted");
            same(&db, "live", |field| json!([order(field, descending)]));
        }
        clean(&db);
        // Another type's index holds nothing of it, and a number in its place is ordered again
        let (_, visits) = visited(&db, plan("live", &["Other"], json!([order("n", false), limit(3)])));
        assert_eq!(visits, 0, "{what}");
        write(&db, vec![set("Item-odd", 7.into())]);
        let (first, visits) = same(&db, "live", |field| json!([order(field, true), limit(3)]));
        assert_eq!(visits, 3, "{what}: gone, the index orders every value");
        assert_eq!(first, of(&all, &[2, 3, 11]), "{what}");
        clean(&db);
    }
}

#[test]
fn a_string_is_read_where_the_index_s_order_has_it_after_every_number() {
    // One the index has an entry for, one past what it holds an entry for, and one whose zero bytes leave it no key
    // of its own in the order: each is read from the order, in as many entries as the page takes
    let strings: Vec<(&str, Value)> = vec![
        ("a string", "pear".into()),
        ("a string past what an index holds", "p".repeat(300).into()),
        ("a string of zero bytes", "\0".repeat(200).into()),
    ];
    for (what, value) in strings {
        let (_dir, db) = open();
        let all = load(&db, "Item", &mixed());
        write(&db, vec![create("Item", "Item-odd", Some(value.clone()), &[])]);
        db.wait_for_index_builds().unwrap();
        for descending in [false, true] {
            let (_, visits) = same(&db, "live", |field| json!([order(field, descending), limit(3)]));
            assert_eq!(visits, 3, "{what}: three entries are read for the first three");
            let (every, _) = same(&db, "live", |field| json!([order(field, descending)]));
            assert_eq!(every.len(), all.len() + 1, "{what}");
            // And a page goes on from it, and from the entity before it
            for cursor in ["Item-odd", every[every.iter().position(|id| id == "Item-odd").unwrap() - 1].as_str()] {
                same(
                    &db,
                    "live",
                    |field| json!([order(field, descending), { "step": "after", "id": cursor }, limit(2)]),
                );
            }
        }
        clean(&db);
    }
}

/// Strings whose order by the collation isn't their bytes': capitals, accents composed and combining, one another's
/// beginning, a pair the collator holds equal, one of zero bytes with no key of its own in an order, long ones that
/// agree past where a sort key is cut, and beside them numbers, a boolean and entities without a value
fn worded() -> Vec<Option<Value>> {
    let long = |end: &str| Value::from(format!("{}{end}", "long ".repeat(70)));
    vec![
        Some("pear".into()),
        Some("Apple".into()),
        Some(3.into()),
        Some("apple".into()),
        Some("é".into()),
        Some("e\u{301}".into()),
        None,
        Some("eb".into()),
        Some("e".into()),
        Some(long("b")),
        Some("Pear".into()),
        Some(long("a")),
        Some(true.into()),
        Some(format!("ea{}", "\0".repeat(260)).into()),
        Some("pear".into()),
        Some(Value::Nil),
        Some("zebra".into()),
        Some(long("a")),
    ]
}

#[test]
fn a_range_of_strings_is_read_from_the_index_s_order_alone_and_before_an_order() {
    let (_dir, db) = open();
    let all = load(&db, "Item", &worded());
    db.wait_for_index_builds().unwrap();
    let strings = worded().iter().flatten().filter(|value| value.is_str()).count() as u64;
    let range = |field: &str, bounds: &Json| {
        let mut step = bounds.clone();
        step["step"] = json!("range");
        step["field"] = json!(field);
        step
    };
    let long = format!("{}a", "long ".repeat(70));
    let bounds = [
        // A bound on either side, within and without; one equal to the collator and of other bytes than a string
        // held; one no string holds; one whose sort key is cut where the long strings' are; and ones that keep none
        json!({ "atLeast": "e" }),
        json!({ "above": "e" }),
        json!({ "atMost": "e\u{301}" }),
        json!({ "below": "é" }),
        json!({ "atLeast": "apple", "atMost": "Pear" }),
        json!({ "above": "Apple", "below": "pear" }),
        json!({ "atLeast": "b", "below": "f" }),
        json!({ "atLeast": long }),
        json!({ "above": long }),
        json!({ "atMost": long }),
        json!({ "below": long }),
        json!({ "atLeast": format!("{long}a") }),
        json!({ "above": "zebra" }),
        json!({ "below": "a" }),
        json!({ "atLeast": "" }),
    ];
    for bound in &bounds {
        // Alone: the entities within it, in creation order, and no entity read but those and what else is in the
        // run at each bound
        let (kept, visits) = same(&db, "live", |field| json!([range(field, bound)]));
        assert!(visits <= kept.len() as u64 + 4, "{bound}: {visits} read for {}", kept.len());
        assert!(visits < strings || kept.len() as u64 >= strings - 1, "{bound}: {visits} of {strings} strings read");
        // Counted, which reads no entity's row but an unplaced string's in a run at a bound
        let counted = |field: &str| {
            let mut asked = plan("live", &["Item"], json!([range(field, bound)]));
            asked["terminal"] = json!({ "answer": "count" });
            read(&db, json!({ "read": "query", "plan": asked })).unwrap()["n"].as_u64().unwrap()
        };
        assert_eq!((counted("n"), counted("m")), (kept.len() as u64, kept.len() as u64), "{bound}");
        // Before an order, either way, a page of it and a page after each entity it keeps
        for descending in [false, true] {
            let (ordered, _) = same(&db, "live", |field| json!([range(field, bound), order(field, descending)]));
            assert_eq!(ordered.len(), kept.len(), "{bound}");
            let (_, visits) =
                same(&db, "live", |field| json!([range(field, bound), order(field, descending), limit(2)]));
            assert!(visits <= 2 + 4, "{bound}, descending {descending}: {visits} read for a page of two");
            for cursor in &ordered {
                same(&db, "live", |field| {
                    json!([range(field, bound), order(field, descending), after(cursor), limit(2)])
                });
            }
        }
    }
    // A range of strings holds no number, and one of numbers no string
    let (kept, _) = same(&db, "live", |field| json!([range(field, &json!({ "atLeast": 0 }))]));
    assert_eq!(kept, of(&all, &[2]));
    clean(&db);
    // With the trash, and of the trash alone, which no index is read for
    write(&db, vec![op(json!({ "op": "trash", "id": all[0] })), op(json!({ "op": "trash", "id": all[4] }))]);
    for scope in ["live", "withTrashed", "trashed"] {
        for bound in &bounds {
            same(&db, scope, |field| json!([range(field, bound)]));
            same(&db, scope, |field| json!([range(field, bound), order(field, true), limit(3)]));
        }
    }
}

#[test]
fn a_long_string_leaves_a_range_of_its_field_s_numbers_to_the_index() {
    let (_dir, db) = open();
    let all = load(&db, "Item", &mixed());
    write(&db, vec![create("Item", "Item-long", Some("p".repeat(300).into()), &[])]);
    // A string is no number, so the range holds none of it: its entries are read, and no entity besides
    let range = |field: &str| json!({ "step": "range", "field": field, "atLeast": 1, "below": 3 });
    let (held, visits) = same(&db, "live", |field| json!([range(field)]));
    assert_eq!((held, visits), (of(&all, &[1, 4, 6, 13]), 4));
    // And the range's order is the index's
    let (first, visits) = same(&db, "live", |field| json!([range(field), order(field, true), limit(2)]));
    assert_eq!((first, visits), (of(&all, &[4, 1]), 2));
    clean(&db);
}

#[test]
fn an_order_from_the_index_reaches_what_the_scope_does() {
    let (_dir, db) = open();
    let all = load(&db, "Item", &mixed());
    // The least and the greatest number, one of three equal numbers, a boolean and one without a value
    let trashed = [8, 12, 6, 5, 3];
    write(&db, trashed.iter().map(|at| op(json!({ "op": "trash", "id": all[*at] }))).collect());
    for descending in [false, true] {
        let (live, _) = same(&db, "live", |field| json!([order(field, descending)]));
        assert_eq!(live.len(), all.len() - trashed.len());
        assert!(trashed.iter().all(|at| !live.contains(&all[*at])));
        let (both, _) = same(&db, "withTrashed", |field| json!([order(field, descending)]));
        assert_eq!(both.len(), all.len());
        let (first, visits) = same(&db, "withTrashed", |field| json!([order(field, descending), limit(4)]));
        assert_eq!(first, both[..4]);
        assert_eq!(visits, 4, "descending {descending}: with the trash, four are read for four");
        // The trash alone is walked and sorted, which reads the trash and not the index of every entity
        let (trash, visits) = same(&db, "trashed", |field| json!([order(field, descending), limit(4)]));
        assert_eq!(trash.len(), 4);
        assert_eq!(visits as usize, trashed.len(), "descending {descending}");
    }
    // An entity in the trash is passed over where it stands in the order
    let (first, visits) = same(&db, "live", |field| json!([order(field, false), limit(3)]));
    assert_eq!(first, of(&all, &[7, 16, 15]));
    assert_eq!(visits, 5, "false, the trashed true, true, the trashed -4, -0.5");
}

#[test]
fn a_page_of_an_order_costs_its_length_wherever_its_cursor_is() {
    let (_dir, db) = open();
    let all = load(&db, "Item", &mixed());
    load(&db, "Other", &[Some(1.into()), None]);
    write(&db, vec![op(json!({ "op": "trash", "id": all[9] }))]);
    for scope in ["live", "withTrashed"] {
        for descending in [false, true] {
            let (sorted, _) = same(&db, scope, |field| json!([order(field, descending)]));
            for (at, cursor) in sorted.iter().enumerate() {
                let (page, visits) =
                    same(&db, scope, |field| json!([order(field, descending), after(cursor), limit(3)]));
                let rest = &sorted[at + 1..];
                assert_eq!(page, rest[..rest.len().min(3)], "{scope}, descending {descending}, after {cursor}");
                // The trashed entity is read and passed over where a live plan's page reaches it
                let passed = usize::from(scope == "live" && visits as usize == page.len() + 1);
                assert_eq!(visits as usize, page.len() + passed, "{scope}, descending {descending}, after {cursor}");
                let (rest_of, _) = same(&db, scope, |field| json!([order(field, descending), after(cursor)]));
                assert_eq!(rest_of, rest, "{scope}, descending {descending}, all after {cursor}");
            }
        }
    }
    // A cursor that isn't in the set answers nothing, and nothing is read for it: another type's entity, no entity,
    // and one in the trash
    for cursor in ["Other-00", "Item-none", all[9].as_str()] {
        for descending in [false, true] {
            let (page, visits) = same(&db, "live", |field| json!([order(field, descending), after(cursor), limit(3)]));
            assert_eq!((page.len(), visits), (0, 0), "after {cursor}, descending {descending}");
        }
    }
}

/// The ids at these places of `all`
fn of(all: &[String], places: &[usize]) -> Vec<String> {
    places.iter().map(|at| all[*at].clone()).collect()
}

/// The ids after `cursor` in `all`, or none when it isn't among them
fn rest_after<'a>(all: &'a [String], cursor: &str) -> &'a [String] {
    match all.iter().position(|id| id == cursor) {
        Some(at) => &all[at + 1..],
        None => &[],
    }
}

#[test]
fn a_page_of_a_type_is_sought_and_not_walked_to() {
    let (_dir, db) = open();
    let items = load(&db, "Item", &mixed());
    let others = load(&db, "Other", &[Some(1.into()), None, Some(2.into()), None, None]);
    write(&db, [3, 4, 10].iter().map(|at| op(json!({ "op": "trash", "id": items[*at] }))).collect());
    write(&db, vec![op(json!({ "op": "trash", "id": others[1] }))]);
    let every: Vec<&String> = items.iter().chain(&others).collect();
    for scope in ["live", "withTrashed", "trashed"] {
        for types in [vec!["Item"], vec!["Item", "Other"], vec!["Other", "Item"], vec!["Item", "Item"]] {
            let all = ids(&db, plan(scope, &types, json!([])));
            for cursor in &every {
                let rest = rest_after(&all, cursor);
                let (page, visits) = visited(&db, plan(scope, &types, json!([after(cursor), limit(3)])));
                assert_eq!(page, rest[..rest.len().min(3)], "{scope}, {types:?}, after {cursor}");
                assert_eq!(visits as usize, page.len(), "{scope}, {types:?}, after {cursor}: only the page is read");
                assert_eq!(ids(&db, plan(scope, &types, json!([after(cursor)]))), rest, "{scope}, {types:?}");
            }
            let (none, visits) = visited(&db, plan(scope, &types, json!([after("Item-none"), limit(3)])));
            assert_eq!((none.len(), visits), (0, 0), "{scope}, {types:?}: no entity's id");
        }
    }
    // A cursor that a step handed on, not the start, is walked to as before
    let filtered = json!([{ "step": "has", "field": "n" }, after(&items[9]), limit(2)]);
    let (page, visits) = visited(&db, plan("live", &["Item"], filtered));
    assert_eq!(page, of(&items, &[12, 13]));
    assert!(visits > 2, "the walk to the cursor reads what is before it: {visits}");
    // And every id from the start, in the order given
    let given = json!({ "start": { "from": "ids", "ids": [items[5], items[1], items[0], items[2]] },
        "steps": [after(&items[1]), limit(1)], "terminal": { "answer": "ids" } });
    assert_eq!(ids(&db, given), of(&items, &[0]));
}

#[test]
fn a_page_of_a_value_s_holders_is_sought_in_the_index_or_the_walk() {
    let (_dir, db) = open();
    // Tags a, b and none in turn; every other entity has a note; every fifth is a star
    let mut ops = Vec::new();
    let mut all = Vec::new();
    for at in 0..30 {
        let id = format!("Item-{at:02}");
        let mut more: Vec<(&str, Value)> = Vec::new();
        match at % 3 {
            0 => more.push(("tag", "a".into())),
            1 => more.push(("tag", "b".into())),
            _ => {}
        }
        if at % 2 == 0 {
            more.push(("note", "kept".into()));
        }
        ops.push(create("Item", &id, None, &more));
        if at % 5 == 0 {
            ops.push(op(json!({ "op": "grant", "id": id, "role": "star" })));
        }
        all.push(id);
    }
    write(&db, ops);
    load(&db, "Other", &[None, None]);
    write(&db, vec![op(json!({ "op": "trash", "id": all[6] }))]);
    let tagged = json!({ "step": "where", "field": "tag", "value": "a" });
    let noted = json!({ "step": "has", "field": "note" });
    let kept = json!({ "step": "where", "field": "note", "value": "kept" });
    let star = json!({ "step": "withRole", "role": "star" });
    // The steps that read their matches, and whether a page of them is read without what is before its cursor
    let cases = [
        ("an index", json!([tagged]), true),
        ("an index after a filter", json!([noted, tagged]), true),
        ("a walk of the type", json!([kept]), true),
        ("the roles' index", json!([star]), true),
        ("the roles' index after a filter", json!([noted, star]), true),
        ("a filter of what an index read", json!([tagged, noted]), false),
    ];
    for (what, steps, sought) in cases {
        for types in [vec!["Item"], vec!["Other", "Item"]] {
            let all_of = ids(&db, plan("live", &types, steps.clone()));
            assert!(all_of.len() >= 3, "{what}: the case holds some");
            for cursor in &all {
                let mut paged = steps.as_array().unwrap().clone();
                paged.extend([after(cursor), limit(2)]);
                let (page, visits) = visited(&db, plan("live", &types, json!(paged)));
                let rest = rest_after(&all_of, cursor);
                assert_eq!(page, rest[..rest.len().min(2)], "{what}, {types:?}, after {cursor}");
                if sought && what != "a walk of the type" && !rest.is_empty() {
                    // The page's two, the trashed entity it passes over, and those a filter leaves out between them:
                    // never the holders before the cursor, of which the last has nine
                    assert!(visits <= 6, "{what}, after {cursor}: {visits} read for a page of two");
                }
                if sought && rest.is_empty() && !all_of.contains(cursor) {
                    assert_eq!(visits, 0, "{what}: nothing is read for a cursor that isn't in the set, {cursor}");
                }
            }
        }
    }
    // A page deep in an index's matches reads its own entries, where walking to it reads all before
    let deep = json!([tagged, after(&all[24]), limit(1)]);
    assert_eq!(visited(&db, plan("live", &["Item"], deep)), (vec![all[27].clone()], 1));
}

/// The entities of `values` a range of numbers holds, by their ids in `all`
fn holding<'a>(all: &'a [String], values: &[Option<Value>], within: impl Fn(f64) -> bool) -> Vec<&'a String> {
    all.iter()
        .zip(values)
        .filter(|(_, value)| match value {
            Some(Value::Boolean(_)) | None => false,
            Some(value) => value.as_f64().is_some_and(&within),
        })
        .map(|(id, _)| id)
        .collect()
}

#[test]
fn a_range_of_numbers_is_read_from_the_index_in_the_order_the_type_was_made() {
    let (_dir, db) = open();
    let values = mixed();
    let all = load(&db, "Item", &values);
    type Within = fn(f64) -> bool;
    let cases: [(Json, Within); 13] = [
        (json!({ "atLeast": 1 }), |n| n >= 1.0),
        (json!({ "above": 1 }), |n| n > 1.0),
        (json!({ "atMost": 1 }), |n| n <= 1.0),
        (json!({ "below": 1 }), |n| n < 1.0),
        (json!({ "atLeast": 1, "below": 3 }), |n| (1.0..3.0).contains(&n)),
        (json!({ "above": 1, "atMost": 3 }), |n| n > 1.0 && n <= 3.0),
        (json!({ "atLeast": 3, "atMost": 3 }), |n| n == 3.0),
        (json!({ "above": 3, "below": 3 }), |_| false),
        (json!({ "atLeast": 5, "atMost": 2 }), |_| false),
        (json!({ "atLeast": -0.0, "atMost": 0.0 }), |n| n == 0.0),
        (json!({ "atLeast": 1.5, "atMost": 2.5 }), |n| (1.5..=2.5).contains(&n)),
        (json!({ "above": -4.5, "below": -0.25 }), |n| n > -4.5 && n < -0.25),
        (json!({ "atLeast": -1e300, "atMost": 1e300 }), |_| true),
    ];
    for (bounds, within) in cases {
        let step = |field: &str| {
            let mut step = bounds.clone();
            step["step"] = json!("range");
            step["field"] = json!(field);
            json!([step])
        };
        let (found, visits) = same(&db, "live", step);
        let expected = holding(&all, &values, within);
        assert_eq!(found.iter().collect::<Vec<_>>(), expected, "{bounds}");
        assert_eq!(visits as usize, expected.len(), "{bounds}: the entries within the bounds are what is read");
    }
    // The scope's entities: one in the trash is a place the index holds where a live plan reads no entity, and
    // one read with the trash
    write(&db, vec![op(json!({ "op": "trash", "id": all[4] }))]);
    let middle = |field: &str| json!([{ "step": "range", "field": field, "above": 1, "below": 3 }]);
    assert_eq!(same(&db, "live", middle), (vec![], 0));
    assert_eq!(same(&db, "withTrashed", middle), (vec![all[4].clone()], 1));
    assert_eq!(
        same(&db, "trashed", |field| json!([{ "step": "range", "field": field, "atLeast": 0 }])).0,
        of(&all, &[4])
    );
    // After another step it filters what reaches it, and before a filter the index is still what is read
    let some = |field: &str| json!([limit(8), { "step": "range", "field": field, "atLeast": 2 }]);
    assert_eq!(same(&db, "live", some), (vec![all[0].clone()], 8));
    let noted =
        |field: &str| json!([{ "step": "range", "field": field, "atLeast": 3 }, { "step": "lacks", "field": "note" }]);
    assert_eq!(same(&db, "live", noted).1, 4);
    // Two types, each read from its own index or walked
    load(&db, "Other", &[Some(2.into()), Some(9.into()), None]);
    let both = plan("live", &["Other", "Item"], json!([{ "step": "range", "field": "n", "atLeast": 9 }]));
    assert_eq!(visited(&db, both), (vec!["Other-01".to_owned(), all[12].clone()], 2));
    clean(&db);
}

#[test]
fn a_range_holds_a_list_s_numbers_and_timestamps_by_their_milliseconds() {
    let (_dir, db) = open();
    let at = |seconds: u32| Value::Ext(-1, seconds.to_be_bytes().to_vec());
    let values = vec![
        Some(Value::Array(vec![0.into(), 7.into()])),
        Some(5.into()),
        Some(Value::Array(vec![Value::Array(vec![7.into()]), "7".into()])),
        Some(at(2)),
        Some("7".into()),
        Some(Value::Array(vec![])),
        Some(2500.into()),
        Some(Value::Map(vec![("n".into(), 7.into())])),
    ];
    let all = load(&db, "Item", &values);
    let range = |bounds: Json| {
        move |field: &str| {
            let mut step = bounds.clone();
            step["step"] = json!("range");
            step["field"] = json!(field);
            json!([step])
        }
    };
    // A list holds its own numbers, not a list's inside it; a string that reads as a number is a string
    let (found, visits) = same(&db, "live", range(json!({ "atLeast": 6, "atMost": 8 })));
    assert_eq!(found, of(&all, &[0]));
    assert_eq!(visits as usize, all.len(), "an index that holds a list is not what a range reads");
    // Two seconds are 2000 milliseconds
    assert_eq!(same(&db, "live", range(json!({ "atLeast": 2000, "below": 2500 }))).0, of(&all, &[3]));
    assert_eq!(same(&db, "live", range(json!({ "above": 2000 }))).0, of(&all, &[6]));
    // And a bound that is a timestamp is its milliseconds
    let from_two_seconds = |field: &str| Plan {
        scope: Scope::Live,
        start: Start::Types { types: vec!["Item".into()] },
        steps: vec![Step::Range {
            field: field.into(),
            at_least: Some(at(2)),
            above: None,
            at_most: None,
            below: None,
        }],
        terminal: Terminal::Ids,
    };
    let answers = |field: &str| {
        let reads = Reads {
            reads: vec![ReadOp::Query { plan: from_two_seconds(field), visits: false, explain: false }],
            track: None,
        };
        let bytes = db.read(&reads).unwrap();
        serde_json::to_value(rmpv::decode::read_value(&mut &bytes[..]).unwrap()).unwrap()[0]["ids"].clone()
    };
    assert_eq!(answers("n"), json!([all[3], all[6]]));
    assert_eq!(answers("m"), json!([all[3], all[6]]));
}

#[test]
fn a_range_of_strings_is_in_the_collation_s_order() {
    let (_dir, db) = open();
    let notes = ["apple", "Banana", "cherry", "date", "éclair", "fig", "b", "E"];
    let all: Vec<String> = (0..notes.len()).map(|at| format!("Item-{at:02}")).collect();
    let mut ops: Vec<Op> = all
        .iter()
        .zip(notes)
        .map(|(id, note)| create("Item", id, Some(note.into()), &[("tag", note.into())]))
        .collect();
    ops.push(create("Item", "Item-number", Some(3.into()), &[]));
    ops.push(create("Item", "Item-list", Some(Value::Array(vec!["cat".into(), 1.into()])), &[]));
    write(&db, ops);
    let range = |field: &'static str, bounds: Json| {
        let mut step = bounds;
        step["step"] = json!("range");
        step["field"] = json!(field);
        ids(&db, plan("live", &["Item"], json!([step])))
    };
    for field in ["n", "m", "tag"] {
        // The root collation puts b before Banana and é between e and f, where the strings' bytes put capitals first
        // and é last
        let own = |ids: Vec<String>| ids.into_iter().filter(|id| all.contains(id)).collect::<Vec<_>>();
        assert_eq!(own(range(field, json!({ "atLeast": "b", "below": "e" }))), of(&all, &[1, 2, 3, 6]));
        assert_eq!(own(range(field, json!({ "above": "b", "atMost": "E" }))), of(&all, &[1, 2, 3, 7]));
        assert_eq!(own(range(field, json!({ "atLeast": "e", "atMost": "f" }))), of(&all, &[4, 7]));
        assert_eq!(own(range(field, json!({ "above": "date" }))), of(&all, &[4, 5, 7]));
    }
    // A list holds its strings, and a number is outside every range of strings, as a string is outside one of numbers
    assert_eq!(range("n", json!({ "atLeast": "cat", "atMost": "cat" })), ["Item-list"]);
    assert_eq!(range("n", json!({ "atLeast": 0 })), ["Item-number", "Item-list"]);
    assert!(range("n", json!({ "below": "a" })).is_empty());
    // An index holding strings and numbers, and no list, is read for a range of numbers: its numbers end where its
    // strings begin
    write(&db, vec![op(json!({ "op": "destroy", "id": "Item-list" }))]);
    let numbers = plan("live", &["Item"], json!([{ "step": "range", "field": "n", "above": 2 }]));
    assert_eq!(visited(&db, numbers), (vec!["Item-number".to_owned()], 1));
}

#[test]
fn strings_the_collation_holds_equal_are_ordered_by_their_bytes() {
    let (_dir, db) = open();
    // é as one character and as an e with a combining accent: one string to the root collation, of different bytes,
    // the combined one's the lesser
    let (composed, combined) = ("\u{e9}", "e\u{301}");
    let notes = [composed, "f", combined, composed, "d", combined];
    let all: Vec<String> = (0..notes.len()).map(|at| format!("Item-{at:02}")).collect();
    let made = all.iter().zip(notes).map(|(id, note)| create("Item", id, Some(note.into()), &[("tag", note.into())]));
    write(&db, made.collect());
    for field in ["n", "m", "tag"] {
        let by = |descending: bool, more: Vec<Json>| {
            let steps: Vec<Json> = [order(field, descending)].into_iter().chain(more).collect();
            ids(&db, plan("live", &["Item"], Json::Array(steps)))
        };
        // The bytes part what the collation holds equal, and strings of the same bytes stay as they came
        assert_eq!(by(false, vec![]), of(&all, &[4, 2, 5, 0, 3, 1]), "{field}");
        // Descending reverses the values, their bytes with them, and not the ties
        assert_eq!(by(true, vec![]), of(&all, &[1, 0, 3, 2, 5, 4]), "{field}");
        // The first few kept in one pass, and a page past a cursor, are of the same order
        assert_eq!(by(false, vec![limit(3)]), of(&all, &[4, 2, 5]), "{field}");
        assert_eq!(by(false, vec![after(&all[2]), limit(2)]), of(&all, &[5, 0]), "{field}");
        assert_eq!(by(true, vec![after(&all[3]), limit(2)]), of(&all, &[2, 5]), "{field}");
        // The least of the four the collation holds equal is the first of the lesser bytes, and the greatest the
        // first of the greater: a range's bounds are the collation's alone, and hold all four
        let extreme = |answer: &str| {
            let mut plan = plan(
                "live",
                &["Item"],
                json!([{ "step": "range", "field": field, "atLeast": "\u{e9}", "atMost": "\u{e9}" }]),
            );
            plan["terminal"] = json!({ "answer": answer, "field": field });
            read(&db, json!({ "read": "query", "plan": plan })).unwrap()
        };
        assert_eq!(extreme("least"), json!({ "kind": "least", "id": all[2], "value": combined }), "{field}");
        assert_eq!(extreme("greatest"), json!({ "kind": "greatest", "id": all[0], "value": composed }), "{field}");
    }
}

#[test]
fn a_range_without_a_bound_or_with_bounds_of_two_kinds_is_refused() {
    let (_dir, db) = open();
    load(&db, "Item", &mixed());
    let refused = |bounds: Json| {
        let mut step = bounds.clone();
        step["step"] = json!("range");
        step["field"] = json!("n");
        // Straight after the start, after a filter, and under the order that would read it from the index
        for steps in
            [json!([step]), json!([{ "step": "has", "field": "n" }, step]), json!([step, order("n", false), limit(1)])]
        {
            let error = read(&db, json!({ "read": "query", "plan": plan("live", &["Item"], steps.clone()) }))
                .expect_err(&format!("{bounds} is refused"));
            assert_eq!(error.code, ErrorCode::BatchInvalid, "{steps}");
            assert!(error.message.starts_with("range takes "), "{}", error.message);
        }
    };
    refused(json!({}));
    refused(json!({ "atLeast": null }));
    refused(json!({ "atLeast": 1, "above": 0 }));
    refused(json!({ "atMost": 1, "below": 2 }));
    refused(json!({ "atLeast": 1, "atMost": "z" }));
    refused(json!({ "atLeast": "a", "below": 9 }));
    refused(json!({ "atLeast": true }));
    refused(json!({ "below": [1] }));
    refused(json!({ "atMost": { "n": 1 } }));
    // A number that isn't one, which JSON can't say
    let not_a_number = Plan {
        scope: Scope::Live,
        start: Start::Types { types: vec!["Item".into()] },
        steps: vec![Step::Range {
            field: "n".into(),
            at_least: Some(Value::F64(f64::NAN)),
            above: None,
            at_most: None,
            below: None,
        }],
        terminal: Terminal::Ids,
    };
    let reads = Reads { reads: vec![ReadOp::Query { plan: not_a_number, visits: false, explain: false }], track: None };
    assert_eq!(db.read(&reads).unwrap_err().code, ErrorCode::BatchInvalid);
    // Bounds past every number are bounds
    let endless = json!([{ "step": "range", "field": "n", "atLeast": -1e308, "atMost": 1e308 }]);
    assert_eq!(ids(&db, plan("live", &["Item"], endless)).len(), 11);
}

#[test]
fn an_order_of_a_range_is_read_between_its_bounds() {
    let (_dir, db) = open();
    let values = mixed();
    let all = load(&db, "Item", &values);
    let ranged = |field: &str, descending: bool| {
        vec![json!({ "step": "range", "field": field, "atLeast": 0, "below": 3 }), order(field, descending)]
    };
    for descending in [false, true] {
        let (sorted, visits) = same(&db, "live", |field| json!(ranged(field, descending)));
        let expected: Vec<&String> = if descending {
            [4, 1, 6, 13, 9].iter().map(|at| &all[*at]).collect()
        } else {
            [9, 1, 6, 13, 4].iter().map(|at| &all[*at]).collect()
        };
        assert_eq!(sorted.iter().collect::<Vec<_>>(), expected, "descending {descending}");
        assert_eq!(visits, 5);
        for take in 1..=6 {
            let (first, visits) = same(&db, "live", |field| {
                let mut steps = ranged(field, descending);
                steps.push(limit(take));
                json!(steps)
            });
            assert_eq!(first, sorted[..take.min(5)], "descending {descending}, the first {take}");
            assert_eq!(visits as usize, take.min(5), "descending {descending}, the first {take}");
        }
        // A page of it, and none after a cursor the range doesn't hold: a number outside it, and no value
        for (at, cursor) in sorted.iter().enumerate() {
            let (page, visits) = same(&db, "live", |field| {
                let mut steps = ranged(field, descending);
                steps.extend([after(cursor), limit(2)]);
                json!(steps)
            });
            let rest = &sorted[at + 1..];
            assert_eq!(page, rest[..rest.len().min(2)], "descending {descending}, after {cursor}");
            assert_eq!(visits as usize, page.len(), "descending {descending}, after {cursor}");
        }
        for outside in of(&all, &[0, 8, 2, 3, 5]) {
            let (page, visits) = same(&db, "live", |field| {
                let mut steps = ranged(field, descending);
                steps.extend([after(&outside), limit(2)]);
                json!(steps)
            });
            assert_eq!((page.len(), visits), (0, 0), "descending {descending}, after {outside}");
        }
    }
    // A range of strings is no part of an order the index reads, and a range of another field is a filter before it
    let texts =
        |field: &str| json!([{ "step": "range", "field": field, "atLeast": "a" }, order(field, false), limit(1)]);
    assert_eq!(same(&db, "live", texts).0.len(), 0);
    let other = |field: &str| json!([{ "step": "range", "field": if field == "n" { "m" } else { "n" }, "atLeast": 0 }, order(field, false), limit(1)]);
    assert_eq!(same(&db, "live", other).0, of(&all, &[9]));
    // Another field's own numbers bound nothing of this one's
    write(
        &db,
        vec![
            op(json!({ "op": "set", "id": all[8], "fields": { "k": 50 } })),
            op(json!({ "op": "set", "id": all[12], "fields": { "k": 60 } })),
            op(json!({ "op": "set", "id": all[9], "fields": { "k": 1 } })),
        ],
    );
    let by_other =
        |field: &str| json!([{ "step": "range", "field": "k", "atLeast": 40 }, order(field, false), limit(1)]);
    assert_eq!(same(&db, "live", by_other), (of(&all, &[8]), 18));
    // Two ranges are one read and one filter, which the order sorts: the first's bound is kept, and not the second's
    // alone
    let twice = |field: &str| {
        json!([
            { "step": "range", "field": field, "atLeast": 0 },
            { "step": "range", "field": field, "below": 3 },
            order(field, false),
            limit(2)
        ])
    };
    assert_eq!(same(&db, "live", twice).0, of(&all, &[9, 1]));
}

#[test]
fn an_order_of_several_types_or_of_ids_is_sorted() {
    let (_dir, db) = open();
    let items = load(&db, "Item", &mixed());
    let others = load(&db, "Other", &[Some(2.into()), Some((-9).into()), None]);
    for descending in [false, true] {
        let both = |field: &str| plan("live", &["Item", "Other"], json!([order(field, descending), limit(3)]));
        let (indexed, visits) = visited(&db, both("n"));
        assert_eq!(indexed, ids(&db, both("m")), "descending {descending}");
        assert_eq!(visits as usize, items.len() + others.len(), "two types are read whole and sorted");
    }
    let (least, _) = visited(&db, plan("live", &["Item", "Other"], json!([order("n", false), limit(3)])));
    assert_eq!(least, of(&items, &[7, 5, 16]));
    let given = json!({ "start": { "from": "ids", "ids": [items[0], items[8], items[4]] },
        "steps": [order("n", false)], "terminal": { "answer": "ids" } });
    assert_eq!(ids(&db, given), of(&items, &[8, 4, 0]));
}

#[test]
fn an_order_by_the_roles_or_the_short_code_is_sorted() {
    // The engine's own indexes on them hold only what a lookup reads: no entity without a role or a short code
    let (_dir, db) = open();
    let all = load(&db, "Item", &mixed());
    for field in ["role", "shortCode"] {
        for descending in [false, true] {
            let (first, visits) = visited(&db, plan("live", &["Item"], json!([order(field, descending), limit(3)])));
            assert_eq!(first, all[..3], "{field}: none holds one, so the order is the one they were made in");
            assert_eq!(visits as usize, all.len(), "{field}");
        }
    }
    write(&db, vec![op(json!({ "op": "grant", "id": all[4], "role": "star" }))]);
    let starred = ids(&db, plan("live", &["Item"], json!([order("role", false), limit(2)])));
    assert_eq!(starred, of(&all, &[4, 0]));
    let range = plan("live", &["Item"], json!([{ "step": "range", "field": "role", "atLeast": "s" }]));
    assert_eq!(visited(&db, range), (of(&all, &[4]), all.len() as u64));
}

#[test]
fn an_index_accounts_for_every_entity_of_its_type_through_every_write() {
    let (_dir, db) = open();
    let without = |db: &Database| filed(db, Prefix::IndexWithout, "Item", "byN");
    let unordered = |db: &Database| filed(db, Prefix::IndexUnordered, "Item", "byN");
    let expect = |db: &Database, lacking: &[&str], odd: &[&str], what: &str| {
        assert_eq!(without(db), lacking, "without a value: {what}");
        assert_eq!(unordered(db), odd, "not ordered: {what}");
        clean(db);
    };
    // The entities holding a long string, which are apart from those the index doesn't order
    let long = |db: &Database| filed(db, Prefix::IndexLong, "Item", "byN");
    write(
        &db,
        vec![
            create("Item", "Item-none", None, &[]),
            create("Item", "Item-null", Some(Value::Nil), &[]),
            create("Item", "Item-list", Some(Value::Array(vec![4.into()])), &[]),
            create("Item", "Item-five", Some(5.into()), &[]),
            create("Item", "Item-text", Some("five".into()), &[]),
            create("Item", "Item-long", Some("5".repeat(400).into()), &[]),
            create("Item", "Item-flag", Some(false.into()), &[]),
        ],
    );
    expect(&db, &["Item-none", "Item-null"], &["Item-list"], "as created");
    assert_eq!(long(&db), ["Item-long"]);
    // Each other index of the type files each entity by its own field, the times every entity holds leaving none
    // without a value; and the short code's and the roles' file none
    assert_eq!(filed(&db, Prefix::IndexWithout, "Item", "byTag").len(), 7);
    assert!(filed(&db, Prefix::IndexUnordered, "Item", "byTag").is_empty());
    let every = db.snapshot(|s| Ok(super::filed_under(s, "Item", &Prefix::IndexWithout.key().to_vec()).len())).unwrap();
    assert_eq!(every, 2 + 7, "byN's two and byTag's seven");

    write(&db, vec![set("Item-none", 1.into())]);
    expect(&db, &["Item-null"], &["Item-list"], "a value given");
    write(&db, vec![set("Item-none", 2.into())]);
    expect(&db, &["Item-null"], &["Item-list"], "a number for a number");
    write(&db, vec![set("Item-none", Value::Nil)]);
    expect(&db, &["Item-none", "Item-null"], &["Item-list"], "null for a number");
    write(&db, vec![set("Item-none", Value::Array(vec![]))]);
    expect(&db, &["Item-null"], &["Item-none", "Item-list"], "a list for null");
    // A string no longer than an index has an entry for is in its entry and not among the long ones, whatever its
    // zero bytes make of its key; one a byte past that length is among them, with no entry
    write(&db, vec![set("Item-none", "\0".repeat(200).into())]);
    expect(&db, &["Item-null"], &["Item-list"], "a string of zero bytes for a list");
    assert_eq!(long(&db), ["Item-long"]);
    write(&db, vec![set("Item-none", "n".repeat(257).into())]);
    expect(&db, &["Item-null"], &["Item-list"], "a string a byte longer");
    assert_eq!(long(&db), ["Item-none", "Item-long"]);
    write(&db, vec![set("Item-none", "text".into())]);
    expect(&db, &["Item-null"], &["Item-list"], "a string for a long one");
    assert_eq!(long(&db), ["Item-long"]);
    write(&db, vec![set("Item-long", Value::Map(vec![]))]);
    expect(&db, &["Item-null"], &["Item-list", "Item-long"], "a map for a long string");
    assert!(long(&db).is_empty());
    write(&db, vec![set("Item-long", true.into())]);
    expect(&db, &["Item-null"], &["Item-list"], "a boolean for a map");
    write(&db, vec![op(json!({ "op": "delete", "id": "Item-list", "field": "n" }))]);
    expect(&db, &["Item-null", "Item-list"], &[], "a list removed");
    write(&db, vec![op(json!({ "op": "delete", "id": "Item-five", "field": "n" }))]);
    expect(&db, &["Item-null", "Item-list", "Item-five"], &[], "a number removed");
    write(&db, vec![op(json!({ "op": "append", "id": "Item-five", "field": "n", "value": 5 }))]);
    expect(&db, &["Item-null", "Item-list"], &["Item-five"], "a list appended to");

    // The trash moves nothing, and a destroyed entity is filed nowhere, whatever its field held
    write(&db, vec![op(json!({ "op": "trash", "id": "Item-null" })), op(json!({ "op": "trash", "id": "Item-five" }))]);
    expect(&db, &["Item-null", "Item-list"], &["Item-five"], "in the trash");
    for id in ["Item-null", "Item-list", "Item-five", "Item-text", "Item-flag"] {
        write(&db, vec![op(json!({ "op": "destroy", "id": id }))]);
    }
    expect(&db, &[], &[], "destroyed");
    assert_eq!(filed(&db, Prefix::IndexWithout, "Item", "byTag"), ["Item-none", "Item-long"]);

    // An upsert that makes an entity files it as a create does, and one that writes it as a set does
    write(&db, vec![op(json!({ "op": "upsert", "type": "Item", "id": "Item-up", "fields": { "tag": "t" } }))]);
    expect(&db, &["Item-up"], &[], "made by an upsert");
    assert_eq!(filed(&db, Prefix::IndexWithout, "Item", "byTag"), ["Item-none", "Item-long"]);
    write(&db, vec![op(json!({ "op": "upsert", "type": "Item", "id": "Item-up", "fields": { "n": 3 } }))]);
    expect(&db, &[], &[], "written by an upsert");
    // A batch refused part way keeps none of what it filed
    let refused = db.write(
        &Batch {
            ops: vec![create("Item", "Item-kept", None, &[]), op(json!({ "op": "destroy", "id": "Item-nobody" }))],
            now: None,
        },
        false,
    );
    assert!(refused.is_err());
    expect(&db, &[], &[], "a refused batch");
}

#[test]
fn an_index_declared_over_entities_or_built_again_accounts_for_each() {
    let mut bare = schema_json();
    bare["types"]["Item"]["indexes"].as_object_mut().unwrap().remove("byN");
    let (_dir, db) = open_with(bare);
    let mut values = mixed();
    values.push(Some(Value::Array(vec![2.into()])));
    let all = load(&db, "Item", &values);
    assert!(filed(&db, Prefix::IndexWithout, "Item", "byN").is_empty());
    let (_, visits) = visited(&db, plan("live", &["Item"], json!([order("n", false), limit(2)])));
    assert_eq!(visits as usize, all.len(), "no index: the type is sorted");

    db.set_schema(Schema::from_json(&schema_json()).unwrap()).unwrap();
    db.wait_for_index_builds().unwrap();
    let lacking: Vec<&String> = [2, 3, 11, 14].iter().map(|at| &all[*at]).collect();
    assert_eq!(filed(&db, Prefix::IndexWithout, "Item", "byN").iter().collect::<Vec<_>>(), lacking);
    assert_eq!(filed(&db, Prefix::IndexUnordered, "Item", "byN"), of(&all, &[18]));
    clean(&db);
    write(&db, vec![set(&all[18], 2.into())]);
    let (first, visits) = same(&db, "live", |field| json!([order(field, false), limit(2)]));
    assert_eq!((first, visits), (vec![all[7].clone(), all[5].clone()], 2), "built, the index is read");

    // Each way of building it again leaves what the writes kept
    for index in [Some(("Item", "byN")), None] {
        db.rebuild(index).unwrap();
        db.wait_for_index_builds().unwrap();
        assert_eq!(filed(&db, Prefix::IndexWithout, "Item", "byN").iter().collect::<Vec<_>>(), lacking, "{index:?}");
        assert!(filed(&db, Prefix::IndexUnordered, "Item", "byN").is_empty(), "{index:?}");
        clean(&db);
        assert_eq!(same(&db, "live", |field| json!([order(field, true), limit(5)])).1, 5, "{index:?}");
    }

    // A unique index accounts for its type's entities too, any number of them without a value
    let mut unique = schema_json();
    unique["types"]["Item"]["indexes"]["byN"]["unique"] = json!(true);
    let (_dir, db) = open_with(unique);
    write(
        &db,
        vec![
            create("Item", "Item-a", Some(1.into()), &[]),
            create("Item", "Item-b", None, &[]),
            create("Item", "Item-c", None, &[]),
            create("Item", "Item-d", Some(Value::Array(vec![7.into(), 8.into()])), &[]),
        ],
    );
    for index in [Some(("Item", "byN")), None] {
        db.rebuild(index).unwrap();
        assert_eq!(filed(&db, Prefix::IndexWithout, "Item", "byN"), ["Item-b", "Item-c"], "{index:?}");
        assert_eq!(filed(&db, Prefix::IndexUnordered, "Item", "byN"), ["Item-d"], "{index:?}");
        clean(&db);
    }
    // And an index a schema drops takes what it filed with it
    let mut dropped = schema_json();
    dropped["types"]["Item"]["indexes"].as_object_mut().unwrap().remove("byN");
    db.set_schema(Schema::from_json(&dropped).unwrap()).unwrap();
    assert!(filed(&db, Prefix::IndexWithout, "Item", "byN").is_empty());
    assert!(filed(&db, Prefix::IndexUnordered, "Item", "byN").is_empty());
    assert_eq!(db.snapshot(|s| Ok(s.scan(&keys::index_range("Item", "byN", &[]))?.len())).unwrap(), 0);
    clean(&db);
}

#[test]
fn explain_names_the_index_an_order_and_a_range_read() {
    let (_dir, db) = open();
    load(&db, "Item", &mixed());
    let stages = |steps: Json| {
        let answer =
            read(&db, json!({ "read": "query", "explain": true, "plan": plan("live", &["Item"], steps) })).unwrap();
        answer["stages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|stage| {
                let mut stage = stage.clone();
                stage.as_object_mut().unwrap().remove("ms");
                stage
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(
        stages(json!([order("n", true), limit(6)])),
        vec![
            json!({ "stage": "types", "visited": 0, "kept": 0 }),
            json!({ "stage": "orderBy", "index": "byN", "visited": 6, "kept": 6 }),
            json!({ "stage": "limit", "visited": 6, "kept": 6 }),
            json!({ "stage": "ids", "visited": 6, "kept": 6 }),
        ]
    );
    assert_eq!(
        stages(json!([{ "step": "range", "field": "n", "atLeast": 3 }])),
        vec![
            json!({ "stage": "types", "visited": 0, "kept": 0 }),
            // Its four places are read from the index, and then walked as a list
            json!({ "stage": "range", "index": "byN", "places": 4 + 4, "visited": 4, "kept": 4 }),
            json!({ "stage": "ids", "visited": 4, "kept": 4 }),
        ]
    );
    // A range the order reads between is the order's stage's work, as a filter before a driving index is its
    assert_eq!(
        stages(json!([{ "step": "range", "field": "n", "atLeast": 3 }, order("n", false), limit(2)])),
        vec![
            json!({ "stage": "types", "visited": 0, "kept": 0 }),
            json!({ "stage": "range", "visited": 0, "kept": 0 }),
            json!({ "stage": "orderBy", "index": "byN", "visited": 2, "kept": 2 }),
            json!({ "stage": "limit", "visited": 2, "kept": 2 }),
            json!({ "stage": "ids", "visited": 2, "kept": 2 }),
        ]
    );
    assert_eq!(
        stages(json!([{ "step": "range", "field": "m", "atLeast": 3 }]))[1],
        json!({ "stage": "range", "index": "none: walks the type", "places": 18, "visited": 18, "kept": 4 })
    );
}

#[test]
fn a_tracked_order_is_touched_by_what_would_change_its_page_and_by_nothing_past_it() {
    let (_dir, db) = open();
    let all = load(&db, "Item", &[Some(5.into()), Some(7.into()), Some(9.into()), Some(11.into())]);
    let track = |steps: Json, token: u32| {
        let query = json!({ "read": "query", "plan": plan("live", &["Item"], steps) });
        let reads = Reads { reads: vec![serde_json::from_value::<ReadOp>(query).unwrap()], track: Some(token) };
        db.read(&reads).unwrap();
    };
    let touched = |ops: Vec<Op>| db.write(&Batch { ops, now: Some(2000.0) }, false).unwrap().1.touched;
    // The two least, the one greatest, and a page of one after the second least
    track(json!([order("n", false), limit(2)]), 1);
    track(json!([order("n", true), limit(1)]), 2);
    track(json!([order("n", false), after(&all[1]), limit(1)]), 3);
    track(json!([{ "step": "range", "field": "n", "atLeast": 6, "atMost": 8 }]), 4);

    // Past the least two and the page, outside the range, and below the greatest, whose key is all the
    // greatest-first order read
    assert!(touched(vec![create("Item", "Item-ten", Some(10.into()), &[])]).is_empty());
    // Among the least two, and inside the range
    assert_eq!(touched(vec![create("Item", "Item-six", Some(6.into()), &[])]), [1, 4]);
    // The page after 7 is 9: one between them changes it
    assert_eq!(touched(vec![create("Item", "Item-eight", Some(8.into()), &[])]), [3, 4]);
    // And one between them past the range is a write into the page alone: below what the greatest-first order read
    assert_eq!(touched(vec![create("Item", "Item-more", Some(8.75.into()), &[])]), [3]);
    // A new greatest, which the least two and the page end before
    assert_eq!(touched(vec![create("Item", "Item-most", Some(99.into()), &[])]), [2]);
    // One without a value is the first of the greatest-first order, and the last of the other, past its page
    assert_eq!(touched(vec![create("Item", "Item-none", None, &[])]), [2]);
    // A value the index doesn't order has every order sorted, and the range walk the type
    assert_eq!(touched(vec![create("Item", "Item-list", Some(Value::Array(vec![1.into()])), &[])]), [1, 2, 3, 4]);
    for token in 1..=4 {
        db.untrack(token);
    }
    write(&db, vec![op(json!({ "op": "destroy", "id": "Item-list" }))]);
    track(json!([order("n", false), limit(2)]), 5);
    track(json!([order("n", true), limit(1)]), 6);
    // A string, which the index holds and doesn't order as the collation does
    assert_eq!(touched(vec![create("Item", "Item-text", Some("z".into()), &[])]), [5, 6]);
}

#[test]
fn the_times_the_engine_writes_are_read_in_order_from_their_indexes() {
    let (_dir, db) = open();
    let made = [50.0, 10.0, 40.0, 10.0, 30.0];
    let all: Vec<String> = (0..made.len()).map(|at| format!("Item-{at:02}")).collect();
    let ops = all
        .iter()
        .zip(made)
        .map(|(id, at)| Op::Create {
            entity_type: "Item".into(),
            id: Some(id.clone()),
            fields: BTreeMap::new(),
            short_code: None,
            created_at: Some(at),
            updated_at: None,
        })
        .collect();
    write(&db, ops);
    let first = |field: &str, descending: bool, take: usize| {
        visited(&db, plan("live", &["Item"], json!([order(field, descending), limit(take)])))
    };
    assert_eq!(first("createdAt", false, 3), (of(&all, &[1, 3, 4]), 3));
    assert_eq!(first("createdAt", true, 2), (of(&all, &[0, 2]), 2));
    assert_eq!(first("updatedAt", true, 1), (of(&all, &[0]), 1), "written when made, unless said");
    // A write stamps the entity, which is then the one written last
    db.write(
        &Batch { ops: vec![op(json!({ "op": "set", "id": all[3], "fields": { "tag": "t" } }))], now: Some(99.0) },
        false,
    )
    .unwrap();
    assert_eq!(first("updatedAt", true, 2), (of(&all, &[3, 0]), 2));
    let since = plan("live", &["Item"], json!([{ "step": "range", "field": "updatedAt", "above": 45 }]));
    assert_eq!(visited(&db, since), (of(&all, &[0, 3]), 2));
    for index in ["made", "written"] {
        assert!(filed(&db, Prefix::IndexWithout, "Item", index).is_empty(), "{index}");
    }
    clean(&db);
}

#[test]
fn a_load_files_each_entity_as_its_writes_would() {
    use crate::engine::maintenance::Load;

    let (_dir, db) = open();
    let entity = |id: &str, fields: Vec<(&str, Value)>| Load::Entity {
        id: id.into(),
        fields: fields.into_iter().map(|(name, value)| (name.to_owned(), value)).collect(),
        trashed: false,
    };
    db.bulk_load(&[
        entity("Item-none", vec![]),
        entity("Item-null", vec![("n", Value::Nil)]),
        entity("Item-five", vec![("n", 5.into()), ("createdAt", 7.into())]),
        entity("Item-list", vec![("n", Value::Array(vec![5.into()]))]),
    ])
    .unwrap();
    assert_eq!(filed(&db, Prefix::IndexWithout, "Item", "byN"), ["Item-none", "Item-null"]);
    assert_eq!(filed(&db, Prefix::IndexUnordered, "Item", "byN"), ["Item-list"]);
    // A load brings the times or leaves an entity without them
    assert_eq!(filed(&db, Prefix::IndexWithout, "Item", "made"), ["Item-none", "Item-null", "Item-list"]);
    clean(&db);
    // Loaded again over what it made, an entity is written and not made: what its fields hold is where it is filed
    db.bulk_load(&[entity("Item-none", vec![("n", 1.into())]), entity("Item-five", vec![("n", Value::Nil)])]).unwrap();
    assert_eq!(filed(&db, Prefix::IndexWithout, "Item", "byN"), ["Item-null", "Item-five"]);
    clean(&db);
    // And a dump of it, loaded, is filed alike
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("items.dump");
    crate::dump::dump(&db, &file).unwrap();
    let target = dir.path().join("loaded");
    crate::dump::load(&file, &target).unwrap();
    let loaded = Database::open(&target, OpenOptions { syncs: false, ..Default::default() }).unwrap();
    for (prefix, index) in
        [(Prefix::IndexWithout, "byN"), (Prefix::IndexUnordered, "byN"), (Prefix::IndexWithout, "made")]
    {
        assert_eq!(filed(&loaded, prefix, "Item", index), filed(&db, prefix, "Item", index), "{index}");
    }
    clean(&loaded);
}

#[test]
fn a_range_is_counted_in_the_index_without_reading_an_entity() {
    let (_dir, db) = open();
    let values = mixed();
    let all = load(&db, "Item", &values);
    load(&db, "Other", &[Some(2.into()), Some(9.into()), None]);
    // One of three equal numbers, the greatest, and one outside the range
    write(&db, [6, 12, 8].iter().map(|at| op(json!({ "op": "trash", "id": all[*at] }))).collect());
    let count = |scope: &str, types: &[&str], field: &str, bounds: Json| {
        let mut step = bounds;
        step["step"] = json!("range");
        step["field"] = json!(field);
        let mut counting = plan(scope, types, json!([step]));
        let listed = ids(&db, counting.clone()).len() as u64;
        counting["terminal"] = json!({ "answer": "count" });
        let answer = read(&db, json!({ "read": "query", "visits": true, "plan": counting })).unwrap();
        assert_eq!(
            answer["answer"]["n"], listed,
            "{scope}, {types:?}, {field}: the count is of the ids the plan lists"
        );
        (listed, answer["visited"].as_u64().unwrap())
    };
    let from_one = json!({ "atLeast": 1 });
    // Eight are within the bounds, two of them in the trash; the index's eight entries are what is read
    assert_eq!(count("live", &["Item"], "n", from_one.clone()), (6, 8));
    assert_eq!(count("withTrashed", &["Item"], "n", from_one.clone()), (8, 8));
    assert_eq!(count("trashed", &["Item"], "n", from_one.clone()), (2, 8));
    assert_eq!(count("live", &["Item", "Other"], "n", from_one.clone()), (8, 10));
    assert_eq!(count("live", &["Item"], "n", json!({ "above": 1, "below": 3 })), (1, 1));
    assert_eq!(count("live", &["Item"], "n", json!({ "atLeast": 50 })), (0, 0));
    // The twin with no index, and two types of which one has no index, are walked
    assert_eq!(count("live", &["Item"], "m", from_one.clone()), (6, 15));
    assert_eq!(count("live", &["Item", "Other"], "m", from_one.clone()), (8, 17));
    // A range of strings is counted in the index's order of them, which holds none here
    assert_eq!(count("live", &["Item"], "n", json!({ "atLeast": "a" })), (0, 0));
    // An index that holds no number has no entry to count
    assert_eq!(count("live", &["Item"], "tag", from_one), (0, 0));
    // As is an index holding a list, whose entries are its elements'
    write(&db, vec![set(&all[0], Value::Array(vec![1.into(), 2.into()]))]);
    assert_eq!(count("live", &["Item"], "n", json!({ "atLeast": 1, "atMost": 2 })), (3, 15));
    // The count reads the index itself: the range's stage hands it nothing to count
    write(&db, vec![set(&all[0], 3.into())]);
    let counting = json!({ "scope": "live", "start": { "from": "types", "types": ["Item"] },
        "steps": [{ "step": "range", "field": "n", "atLeast": 1 }], "terminal": { "answer": "count" } });
    let explained = read(&db, json!({ "read": "query", "explain": true, "plan": counting })).unwrap();
    assert_eq!(explained["answer"]["n"], 6);
    let stage = |at: usize| (explained["stages"][at]["stage"].clone(), explained["stages"][at]["visited"].clone());
    assert_eq!([stage(1), stage(2)], [(json!("range"), json!(0)), (json!("count"), json!(8))]);
    // And a count after another step counts what the range hands on
    let after_a_filter = plan("live", &["Item"], json!([limit(4), { "step": "range", "field": "n", "atLeast": 1 }]));
    let mut counting = after_a_filter;
    counting["terminal"] = json!({ "answer": "count" });
    assert_eq!(read(&db, json!({ "read": "query", "plan": counting })).unwrap()["n"], 2);
}
