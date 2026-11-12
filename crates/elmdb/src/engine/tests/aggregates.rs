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
