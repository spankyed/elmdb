//! `least`, `greatest` and `sum`: the entity a field is least or greatest at, and a field's sum, over a plan's set.

use serde_json::{Value as Json, json};

use crate::database::{Database, OpenOptions};
use crate::engine::plan::Reads;
use crate::error::ErrorCode;
use crate::schema::Schema;

use super::write;

/// Items with a price (a number), a quantity (an integer), a name (a string), and a price only some of them hold
fn open(dir: &tempfile::TempDir) -> Database {
    let schema = json!({
        "$schema": crate::schema::SCHEMA_LANGUAGE,
        "types": { "Item": { "fields": {
            "price": { "type": "number", "optional": true },
            "qty": { "type": "integer", "optional": true },
            "name": { "type": "string", "optional": true },
            "rank": { "type": "any", "optional": true }
        } } }
    });
    Database::open(
        dir.path(),
        OpenOptions { schema: Some(Schema::from_json(&schema).unwrap()), syncs: false, ..Default::default() },
    )
    .unwrap()
}

fn read(db: &Database, plan: Json) -> crate::Result<Json> {
    let reads =
        Reads { reads: vec![serde_json::from_value(json!({ "read": "query", "plan": plan })).unwrap()], track: None };
    let bytes = db.read(&reads)?;
    let mut answers = serde_json::to_value(rmpv::decode::read_value(&mut &bytes[..]).unwrap()).unwrap();
    Ok(answers[0].take())
}

fn plan(scope: &str, steps: Json, terminal: Json) -> Json {
    json!({ "scope": scope, "start": { "from": "types", "types": ["Item"] }, "steps": steps, "terminal": terminal })
}

/// Nine items. Prices: 5, 2, 9, none (Item-3), 2, 7.5, none (Item-6), 0.5, none (Item-8), so Item-1 and Item-4 tie at 2.
/// `qty` is 1 for all but Item-3 (2) and Item-5 (3); Item-8 holds a `rank` that is null, not a price
fn stocked(db: &Database) {
    let item =
        |i: usize, fields: Json| json!({ "op": "create", "type": "Item", "id": format!("Item-{i}"), "fields": fields });
    write(
        db,
        vec![
            item(0, json!({ "price": 5.0, "qty": 1, "name": "pear" })),
            item(1, json!({ "price": 2.0, "qty": 1, "name": "apple" })),
            item(2, json!({ "price": 9.0, "qty": 1, "name": "plum" })),
            item(3, json!({ "qty": 2, "name": "fig" })),
            item(4, json!({ "price": 2.0, "qty": 1, "name": "kiwi" })),
            item(5, json!({ "price": 7.5, "qty": 3, "name": "lime" })),
            item(6, json!({ "qty": 1, "name": "date" })),
            item(7, json!({ "price": 0.5, "qty": 1, "name": "yam" })),
            // A null is no value: it sorts last in the order, and is left out of every answer. It is held by a field that
            // takes any value, the others take none
            item(8, json!({ "qty": 1, "name": "kale", "rank": null })),
        ],
    )
    .unwrap();
}

#[test]
fn least_and_greatest_answer_the_first_entity_at_the_extreme_value_in_the_plans_order() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    stocked(&db);
    let least = read(&db, plan("live", json!([]), json!({ "answer": "least", "field": "price" }))).unwrap();
    assert_eq!(least, json!({ "kind": "least", "id": "Item-7", "value": 0.5 }));
    let greatest = read(&db, plan("live", json!([]), json!({ "answer": "greatest", "field": "price" }))).unwrap();
    assert_eq!(greatest, json!({ "kind": "greatest", "id": "Item-2", "value": 9.0 }));
    // Ties: the first of two 2s in the plan's order (Item-1, not Item-4), from the least and from the greatest alike
    let tied = read(&db, plan("live", json!([{ "step": "where", "field": "qty", "value": 1 }, { "step": "range", "field": "price", "atLeast": 2.0, "below": 3.0 }]), json!({ "answer": "least", "field": "price" }))).unwrap();
    assert_eq!(tied, json!({ "kind": "least", "id": "Item-1", "value": 2.0 }));
    let tied_greatest = read(
        &db,
        plan(
            "live",
            json!([{ "step": "range", "field": "price", "atLeast": 2.0, "below": 3.0 }]),
            json!({ "answer": "greatest", "field": "price" }),
        ),
    )
    .unwrap();
    assert_eq!(tied_greatest, json!({ "kind": "greatest", "id": "Item-1", "value": 2.0 }));
}

#[test]
fn an_entity_without_the_field_is_no_least_or_greatest_and_none_holding_one_answers_null() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    stocked(&db);
    // Item-3 is the one set by qty 2 to 3, and it holds no price: the greatest of a set with none holding one is none
    let none =
        json!([{ "step": "range", "field": "qty", "atLeast": 2, "below": 3 }, { "step": "lacks", "field": "price" }]);
    assert_eq!(
        read(&db, plan("live", none.clone(), json!({ "answer": "least", "field": "price" }))).unwrap(),
        json!({ "kind": "least", "id": null, "value": null })
    );
    assert_eq!(
        read(&db, plan("live", none, json!({ "answer": "greatest", "field": "price" }))).unwrap(),
        json!({ "kind": "greatest", "id": null, "value": null })
    );
    // A set of no entities at all
    let nothing = json!([{ "step": "where", "field": "name", "value": "nobody" }]);
    assert_eq!(
        read(&db, plan("live", nothing, json!({ "answer": "greatest", "field": "price" }))).unwrap()["id"],
        json!(null)
    );
}

#[test]
fn a_string_field_is_ordered_as_orderby_orders_it_and_a_trashed_entity_is_in_scope_only_when_asked() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    stocked(&db);
    // The name's order is the collation's: apple is least, and plum greatest
    let least = read(&db, plan("live", json!([]), json!({ "answer": "least", "field": "name" }))).unwrap();
    assert_eq!(least["id"], json!("Item-1"));
    let greatest = read(&db, plan("live", json!([]), json!({ "answer": "greatest", "field": "name" }))).unwrap();
    assert_eq!(greatest["id"], json!("Item-7"), "yam is the greatest name of the nine");
    // Trash the least price: live it is gone from the answer, and withTrashed it is still there
    write(&db, vec![json!({ "op": "trash", "id": "Item-7" })]).unwrap();
    assert_eq!(
        read(&db, plan("live", json!([]), json!({ "answer": "least", "field": "price" }))).unwrap()["id"],
        json!("Item-1")
    );
    assert_eq!(
        read(&db, plan("withTrashed", json!([]), json!({ "answer": "least", "field": "price" }))).unwrap()["id"],
        json!("Item-7")
    );
}

#[test]
fn sum_adds_the_numbers_the_set_holds_and_counts_them() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    stocked(&db);
    // Six prices hold 5 + 2 + 9 + 2 + 7.5 + 0.5 = 26; the three items without one (Item-3, Item-6, Item-8) are left out
    let all = read(&db, plan("live", json!([]), json!({ "answer": "sum", "field": "price" }))).unwrap();
    assert_eq!(all, json!({ "kind": "sum", "sum": 26.0, "n": 6 }));
    // An integer field sums the same way: 1+1+1+2+1+3+1+1+1 = 12 over all nine items
    let qty = read(&db, plan("live", json!([]), json!({ "answer": "sum", "field": "qty" }))).unwrap();
    assert_eq!(qty, json!({ "kind": "sum", "sum": 12.0, "n": 9 }));
    // Of a filter's set only
    let some = read(
        &db,
        plan(
            "live",
            json!([{ "step": "where", "field": "qty", "value": 1 }]),
            json!({ "answer": "sum", "field": "price" }),
        ),
    )
    .unwrap();
    assert_eq!(
        some,
        json!({ "kind": "sum", "sum": 18.5, "n": 5 }),
        "Item-8's null rank is no price, and Item-6 holds none"
    );
    // A set of none sums to zero over none
    let none = read(
        &db,
        plan(
            "live",
            json!([{ "step": "where", "field": "name", "value": "nobody" }]),
            json!({ "answer": "sum", "field": "price" }),
        ),
    )
    .unwrap();
    assert_eq!(none, json!({ "kind": "sum", "sum": 0.0, "n": 0 }));
}

#[test]
fn a_sum_over_a_value_that_is_no_number_refuses_the_plan_and_names_the_entity() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    stocked(&db);
    let error = read(&db, plan("live", json!([]), json!({ "answer": "sum", "field": "name" }))).unwrap_err();
    assert_eq!(error.code, ErrorCode::ValueInvalid, "{}", error.message);
    assert!(
        error.message.contains("sum(name)")
            && error.message.contains("Item-0")
            && error.message.contains("holds a string"),
        "{}",
        error.message
    );
}

#[test]
fn a_sum_past_what_a_number_holds_refuses_the_plan() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    write(
        &db,
        vec![
            json!({ "op": "create", "type": "Item", "id": "Item-0", "fields": { "price": 1e308 } }),
            json!({ "op": "create", "type": "Item", "id": "Item-1", "fields": { "price": 1e308 } }),
        ],
    )
    .unwrap();
    let error = read(&db, plan("live", json!([]), json!({ "answer": "sum", "field": "price" }))).unwrap_err();
    assert_eq!(error.code, ErrorCode::ValueInvalid, "{}", error.message);
    // The greatest of the same set is still answered: it is a comparison, not a sum
    assert_eq!(
        read(&db, plan("live", json!([]), json!({ "answer": "greatest", "field": "price" }))).unwrap()["value"],
        json!(1e308)
    );
}

#[test]
fn a_null_is_no_value_for_least_or_greatest_and_is_left_out_of_both() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    write(
        &db,
        vec![
            json!({ "op": "create", "type": "Item", "id": "Item-0", "fields": { "rank": null } }),
            json!({ "op": "create", "type": "Item", "id": "Item-1", "fields": { "rank": 4 } }),
            json!({ "op": "create", "type": "Item", "id": "Item-2", "fields": { "rank": null } }),
            json!({ "op": "create", "type": "Item", "id": "Item-3", "fields": { "rank": 2 } }),
        ],
    )
    .unwrap();
    // The null sorts past every number in orderBy, so a null that were a value would be the greatest
    let greatest = read(&db, plan("live", json!([]), json!({ "answer": "greatest", "field": "rank" }))).unwrap();
    assert_eq!(greatest, json!({ "kind": "greatest", "id": "Item-1", "value": 4 }));
    let least = read(&db, plan("live", json!([]), json!({ "answer": "least", "field": "rank" }))).unwrap();
    assert_eq!(least, json!({ "kind": "least", "id": "Item-3", "value": 2 }));
    // With nothing but nulls, none holds a value
    let nulls = json!([{ "step": "where", "field": "rank", "value": null }]);
    assert_eq!(
        read(&db, plan("live", nulls, json!({ "answer": "greatest", "field": "rank" }))).unwrap()["id"],
        json!(null)
    );
}

#[test]
fn a_sum_over_a_boolean_refuses_the_plan_and_names_its_kind() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    write(&db, vec![json!({ "op": "create", "type": "Item", "id": "Item-0", "fields": { "rank": true } })]).unwrap();
    let error = read(&db, plan("live", json!([]), json!({ "answer": "sum", "field": "rank" }))).unwrap_err();
    assert_eq!(error.code, ErrorCode::ValueInvalid, "{}", error.message);
    assert!(error.message.contains("holds a boolean") && error.message.contains("Item-0"), "{}", error.message);
}

#[test]
fn a_sum_past_what_a_number_holds_is_refused_as_it_passes_not_as_it_ends() {
    // The running total passes the largest number, though a later value would bring it back: refused
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    write(
        &db,
        vec![
            json!({ "op": "create", "type": "Item", "id": "Item-0", "fields": { "price": 1e308 } }),
            json!({ "op": "create", "type": "Item", "id": "Item-1", "fields": { "price": 1e308 } }),
            json!({ "op": "create", "type": "Item", "id": "Item-2", "fields": { "price": -1e308 } }),
        ],
    )
    .unwrap();
    let error = read(&db, plan("live", json!([]), json!({ "answer": "sum", "field": "price" }))).unwrap_err();
    assert_eq!(error.code, ErrorCode::ValueInvalid, "{}", error.message);
    assert!(error.message.contains("passes what a number holds"), "{}", error.message);
}

#[test]
fn a_sum_keeps_what_each_addition_rounds_off() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    let item =
        |i: usize, fields: Json| json!({ "op": "create", "type": "Item", "id": format!("Item-{i}"), "fields": fields });
    let sum = |steps: Json| {
        read(&db, plan("live", steps, json!({ "answer": "sum", "field": "price" }))).unwrap()["sum"].clone()
    };
    // Ten tenths are one: added one by one they come to 0.9999999999999999
    write(&db, (0..10).map(|i| item(i, json!({ "price": 0.1, "qty": 1 }))).collect()).unwrap();
    assert_eq!((0..10).fold(0.0, |total, _| total + 0.1), 0.9999999999999999_f64);
    assert_eq!(sum(json!([])), json!(1.0));
    // A one between a large number and its opposite is what is left of the three: added one by one the one is lost to
    // the large number, whichever of the two is the larger as it is added
    write(
        &db,
        vec![
            item(10, json!({ "price": 1e16, "qty": 2 })),
            item(11, json!({ "price": 1.0, "qty": 2 })),
            item(12, json!({ "price": -1e16, "qty": 2 })),
            item(13, json!({ "price": 1.0, "qty": 3 })),
            item(14, json!({ "price": 1e16, "qty": 3 })),
            item(15, json!({ "price": -1e16, "qty": 3 })),
        ],
    )
    .unwrap();
    assert_eq!(1e16 + 1.0 - 1e16, 0.0);
    assert_eq!(sum(json!([{ "step": "where", "field": "qty", "value": 2 }])), json!(1.0));
    assert_eq!(sum(json!([{ "step": "where", "field": "qty", "value": 3 }])), json!(1.0));
    // And of all sixteen, three
    assert_eq!(sum(json!([])), json!(3.0));
}

/// `open`'s schema with an index on `price` and one on `qty` then `price`
fn open_indexed(dir: &tempfile::TempDir) -> Database {
    let schema = json!({
        "$schema": crate::schema::SCHEMA_LANGUAGE,
        "types": { "Item": {
            "fields": {
                "price": { "type": "number", "optional": true },
                "qty": { "type": "integer", "optional": true },
                "name": { "type": "string", "optional": true },
                "rank": { "type": "any", "optional": true }
            },
            "indexes": {
                "byPrice": { "kind": "value", "fields": ["price"] },
                "byQtyPrice": { "kind": "value", "fields": ["qty", "price"] },
                "byName": { "kind": "value", "fields": ["name"] },
                "byRank": { "kind": "value", "fields": ["rank"] }
            }
        } }
    });
    Database::open(
        dir.path(),
        OpenOptions { schema: Some(Schema::from_json(&schema).unwrap()), syncs: false, ..Default::default() },
    )
    .unwrap()
}

/// The stages of a plan's explained read, without their times
fn stages(db: &Database, plan: Json) -> Vec<Json> {
    let reads = Reads {
        reads: vec![serde_json::from_value(json!({ "read": "query", "explain": true, "plan": plan })).unwrap()],
        track: None,
    };
    let bytes = db.read(&reads).unwrap();
    let answers = serde_json::to_value(rmpv::decode::read_value(&mut &bytes[..]).unwrap()).unwrap();
    answers[0]["stages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|stage| {
            let mut stage = stage.clone();
            stage.as_object_mut().unwrap().remove("ms");
            stage
        })
        .collect()
}

#[test]
fn least_and_greatest_read_from_an_index_answer_what_reading_the_set_answers() {
    let (plain_dir, indexed_dir) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let (plain, indexed) = (open(&plain_dir), open_indexed(&indexed_dir));
    for db in [&plain, &indexed] {
        stocked(db);
        // More ties, at the least and at the greatest price, a boolean and a number in one field, and entities in the
        // trash at both ends: the first of the least and of the greatest prices, and the least and greatest alone
        let item = |i: usize, fields: Json| json!({ "op": "create", "type": "Item", "id": format!("Item-{i}"), "fields": fields });
        write(
            db,
            vec![
                item(9, json!({ "price": 0.5, "qty": 2, "rank": 4 })),
                item(10, json!({ "price": 9.0, "qty": 2, "rank": true })),
                item(11, json!({ "price": 9.0, "qty": 1, "rank": false })),
                item(12, json!({ "price": 0.25, "qty": 3, "rank": 4 })),
                item(13, json!({ "price": 12.0, "qty": 3, "rank": -1 })),
                item(14, json!({ "price": -0.0, "qty": 4 })),
                item(15, json!({ "price": 0.0, "qty": 4 })),
            ],
        )
        .unwrap();
        write(db, ["Item-7", "Item-2", "Item-12", "Item-13"].map(|id| json!({ "op": "trash", "id": id })).to_vec())
            .unwrap();
    }
    let filters = [
        json!([]),
        json!([{ "step": "where", "field": "qty", "value": 1 }]),
        json!([{ "step": "where", "field": "qty", "value": 2 }]),
        json!([{ "step": "where", "field": "qty", "value": 3 }]),
        json!([{ "step": "where", "field": "qty", "value": 4 }]),
        json!([{ "step": "where", "field": "qty", "value": 7 }]),
        json!([{ "step": "range", "field": "price", "atLeast": 2.0, "below": 9.0 }]),
        json!([{ "step": "range", "field": "price", "above": 0.5, "atMost": 9.0 }]),
        json!([{ "step": "where", "field": "qty", "value": 1 }, { "step": "range", "field": "price", "atLeast": 2.0 }]),
        json!([{ "step": "range", "field": "price", "atLeast": 100.0 }]),
        // What no index reads in the field's order: another field's filter, and an order before it
        json!([{ "step": "where", "field": "name", "value": "kiwi" }]),
        json!([{ "step": "orderBy", "field": "name", "descending": true }]),
    ];
    let mut read_from_an_index = 0;
    for scope in ["live", "withTrashed", "trashed"] {
        for steps in &filters {
            for field in ["price", "rank", "name", "qty"] {
                for answer in ["least", "greatest"] {
                    let asked = plan(scope, steps.clone(), json!({ "answer": answer, "field": field }));
                    let (from_set, from_index) = (read(&plain, asked.clone()), read(&indexed, asked.clone()));
                    match (from_set, from_index) {
                        (Ok(from_set), Ok(from_index)) => assert_eq!(from_index, from_set, "{asked}"),
                        (Err(from_set), Err(from_index)) => assert_eq!(from_index.code, from_set.code, "{asked}"),
                        (from_set, from_index) => {
                            panic!("{asked}: {from_set:?} of the set, {from_index:?} of an index")
                        }
                    }
                    let terminal = stages(&indexed, asked).pop().unwrap();
                    read_from_an_index += usize::from(terminal.get("index").is_some());
                }
            }
        }
    }
    // Of the 288 reads, those of a set an index orders by the field were read from it: live and with the trash, of
    // `price` under no filter, each of the five equalities on `qty`, the three ranges and the equality with a range
    // (ten sets, two answers, two scopes), and of `rank`, which holds booleans and numbers, under no filter. A name
    // is a string, which no index reads in the collation's order, and no index's last field is `qty`
    assert_eq!(read_from_an_index, 40 + 4);
}

#[test]
fn least_and_greatest_read_from_an_index_read_one_entry_and_those_of_entities_out_of_scope_before_it() {
    let dir = tempfile::tempdir().unwrap();
    let db = open_indexed(&dir);
    stocked(&db);
    let explained =
        |steps: Json, answer: &str| stages(&db, plan("live", steps, json!({ "answer": answer, "field": "price" })));
    // Nine items, and one entry read whichever end is asked for
    for answer in ["least", "greatest"] {
        assert_eq!(
            explained(json!([]), answer),
            vec![
                json!({ "stage": "types", "visited": 0, "kept": 0 }),
                json!({ "stage": answer, "index": "byPrice", "visited": 1, "kept": 1 }),
            ]
        );
    }
    // Under an equality the index of both fields is read, and within a range the index's entries for it
    let of_one = explained(json!([{ "step": "where", "field": "qty", "value": 1 }]), "greatest");
    assert_eq!(of_one.last().unwrap(), &json!({ "stage": "greatest", "index": "byQtyPrice", "visited": 1, "kept": 1 }));
    let within = explained(json!([{ "step": "range", "field": "price", "atLeast": 1.0, "below": 6.0 }]), "greatest");
    assert_eq!(within.last().unwrap(), &json!({ "stage": "greatest", "index": "byPrice", "visited": 1, "kept": 1 }));
    // The two greatest in the trash: their entries are read and passed over, and the third is the answer
    write(&db, vec![json!({ "op": "trash", "id": "Item-2" }), json!({ "op": "trash", "id": "Item-5" })]).unwrap();
    assert_eq!(
        explained(json!([]), "greatest").last().unwrap(),
        &json!({ "stage": "greatest", "index": "byPrice", "visited": 3, "kept": 1 })
    );
    assert_eq!(
        read(&db, plan("live", json!([]), json!({ "answer": "greatest", "field": "price" }))).unwrap(),
        json!({ "kind": "greatest", "id": "Item-0", "value": 5.0 })
    );
    // None holding a value: nothing kept
    assert_eq!(
        explained(json!([{ "step": "range", "field": "price", "atLeast": 100.0 }]), "least").last().unwrap(),
        &json!({ "stage": "least", "index": "byPrice", "visited": 0, "kept": 0 })
    );
}
