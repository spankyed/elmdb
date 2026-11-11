//! An order and a range read under equalities (`docs/architecture/engine.md`, Reads: plans): from an index on the
//! equalities' fields and then the ordered one, or, with no such index, by walking the order and testing each entity
//! against the filters while that is the cheaper way. `n` and `k` are in indexes and their twins `m` and `j`, which
//! every write here gives the same values, in none, so a plan over the one is held to what it answers over the
//! other, which is filtered and then sorted; what a plan visited shows which of the ways it read.

use serde_json::{Value as Json, json};

use crate::engine::keys;
use crate::engine::plan::{ReadOp, Reads};
use crate::engine::unit::Read;
use crate::error::ErrorCode;
use crate::format::Prefix;
use crate::schema::Schema;
use crate::{Database, OpenOptions};

fn schema_json() -> Json {
    json!({
        "$schema": crate::schema::SCHEMA_LANGUAGE,
        "roles": { "star": {}, "lead": {} },
        "types": {
            "Task": {
                "fields": {
                    "status": { "type": "any", "optional": true },
                    "owner": { "type": "integer", "optional": true },
                    "n": { "type": "any", "optional": true },
                    "m": { "type": "any", "optional": true },
                    "k": { "type": "number", "optional": true },
                    "j": { "type": "number", "optional": true },
                    "rank": { "type": "number", "optional": true },
                    "note": { "type": "string", "optional": true }
                },
                "indexes": {
                    "statusN": { "kind": "value", "fields": ["status", "n"] },
                    "ownerStatusN": { "kind": "value", "fields": ["owner", "status", "n"] },
                    "byStatus": { "kind": "value", "fields": ["status"] },
                    "byOwner": { "kind": "value", "fields": ["owner"] },
                    "byK": { "kind": "value", "fields": ["k"] },
                    "ownerRank": { "kind": "value", "fields": ["owner", "rank"] }
                }
            },
            "Other": { "fields": { "status": { "type": "any", "optional": true } } },
            "Small": {
                "fields": { "k": { "type": "number" }, "note": { "type": "string", "optional": true } },
                "indexes": { "byK": { "kind": "value", "fields": ["k"] } }
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

fn write(db: &Database, ops: Vec<Json>) {
    super::write(db, ops).unwrap();
}

fn id(at: usize) -> String {
    format!("Task-{at:03}")
}

/// A task at a place, holding these fields; `n` and `k` are given their twins too
fn task(at: usize, fields: Json) -> Json {
    let mut fields = fields;
    for (field, twin) in [("n", "m"), ("k", "j")] {
        if let Some(value) = fields.get(field).cloned() {
            fields[twin] = value;
        }
    }
    json!({ "op": "create", "type": "Task", "id": id(at), "fields": fields })
}

/// `n` and its twin set on a task
fn set(at: usize, field: &str, value: Json) -> Json {
    let twin = if field == "n" { Some("m") } else { None };
    let mut fields = json!({ field: value.clone() });
    if let Some(twin) = twin {
        fields[twin] = value;
    }
    json!({ "op": "set", "id": id(at), "fields": fields })
}

const STATUSES: [Option<&str>; 6] = [Some("open"), Some("done"), Some("open"), None, Some("open"), Some("done")];

/// Twenty-four tasks, each a combination of its own: a status or none, an owner or none, and in `n` numbers that
/// repeat, booleans, null and nothing
fn mixed(db: &Database) -> Vec<Json> {
    let numbers = [json!(3), json!(1), json!(null), Json::Null, json!(2.5), json!(true), json!(1), json!(false)];
    let tasks: Vec<Json> = (0..24)
        .map(|at| {
            let mut fields = json!({});
            if let Some(status) = STATUSES[at % 6] {
                fields["status"] = json!(status);
            }
            if at % 3 != 2 {
                fields["owner"] = json!(at % 3 + 1);
            }
            // The fourth of eight holds no `n` at all, where the third holds null
            if at % 8 != 3 {
                fields["n"] = numbers[at % 8].clone();
            }
            task(at, fields)
        })
        .collect();
    write(db, tasks.clone());
    tasks
}

/// How many of `mixed`'s tasks hold this status
fn holding(status: &str) -> usize {
    (0..24).filter(|at| STATUSES[at % 6] == Some(status)).count()
}

fn read(db: &Database, op: Json) -> crate::Result<Json> {
    let reads = Reads { reads: vec![serde_json::from_value::<ReadOp>(op).unwrap()], track: None };
    let bytes = db.read(&reads)?;
    let mut answers = serde_json::to_value(rmpv::decode::read_value(&mut &bytes[..]).unwrap()).unwrap();
    Ok(answers[0].take())
}

fn plan(scope: &str, steps: Json) -> Json {
    json!({ "scope": scope, "start": { "from": "types", "types": ["Task"] }, "steps": steps, "terminal": { "answer": "ids" } })
}

/// The ids a plan answers, and the entities it visited
fn visited(db: &Database, plan: Json) -> (Vec<String>, u64) {
    let answer = read(db, json!({ "read": "query", "visits": true, "plan": plan })).unwrap();
    let ids = answer["answer"]["ids"].as_array().unwrap().iter().map(|id| id.as_str().unwrap().to_owned()).collect();
    (ids, answer["visited"].as_u64().unwrap())
}

/// What a plan of tasks answers over the indexed field, and what it visited, once that is what it answers over the
/// field's twin: `steps` is given `n` and then `m`, or `k` and then `j`
fn same(db: &Database, scope: &str, fields: (&str, &str), steps: impl Fn(&str) -> Json) -> (Vec<String>, u64) {
    let (indexed, visits) = visited(db, plan(scope, steps(fields.0)));
    let (plain, _) = visited(db, plan(scope, steps(fields.1)));
    assert_eq!(indexed, plain, "{scope}: {}", steps(fields.0));
    (indexed, visits)
}

const N: (&str, &str) = ("n", "m");
const K: (&str, &str) = ("k", "j");

fn is(field: &str, value: Json) -> Json {
    json!({ "step": "where", "field": field, "value": value })
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

/// The ids an index files under `prefix`, in the order of what leads them and then by place
fn filed(db: &Database, prefix: Prefix, index: &str) -> Vec<String> {
    db.snapshot(|s| {
        Ok(s.scan(&keys::index_others(prefix, "Task", index, &[]))?
            .into_iter()
            .map(|(_, id)| rmpv::decode::read_value(&mut &id[..]).unwrap().as_str().unwrap().to_owned())
            .collect())
    })
    .unwrap()
}

fn clean(db: &Database) {
    let report = db.check().unwrap();
    assert!(report.is_clean(), "{report:?}");
}

#[test]
fn an_order_under_an_equality_is_read_from_the_index_on_both() {
    let (_dir, db) = open();
    mixed(&db);
    for status in ["open", "done"] {
        let all = holding(status);
        for descending in [false, true] {
            let steps = |field: &str| vec![is("status", json!(status)), order(field, descending)];
            let (sorted, visits) = same(&db, "live", N, |field| json!(steps(field)));
            assert_eq!((sorted.len(), visits as usize), (all, all), "{status}, descending {descending}");
            for take in [1, 2, 3, 5, all, all + 2] {
                let (first, visits) = same(&db, "live", N, |field| {
                    let mut steps = steps(field);
                    steps.push(limit(take));
                    json!(steps)
                });
                let kept = take.min(all);
                assert_eq!(first, sorted[..kept], "{status}, descending {descending}, the first {take}");
                assert_eq!(visits as usize, kept, "{status}, descending {descending}: {take} read for {take}");
            }
            // A page after each of them costs its length, and one after a task the equality doesn't keep holds none
            for (at, cursor) in sorted.iter().enumerate() {
                let (page, visits) = same(&db, "live", N, |field| {
                    let mut steps = steps(field);
                    steps.extend([after(cursor), limit(2)]);
                    json!(steps)
                });
                let rest = &sorted[at + 1..];
                assert_eq!(page, rest[..rest.len().min(2)], "{status}, descending {descending}, after {cursor}");
                assert_eq!(visits as usize, page.len(), "{status}, descending {descending}, after {cursor}");
            }
            for outside in [id(3), id(if status == "open" { 1 } else { 0 }), "Task-none".to_owned()] {
                let (page, visits) = same(&db, "live", N, |field| {
                    let mut steps = steps(field);
                    steps.extend([after(&outside), limit(2)]);
                    json!(steps)
                });
                assert_eq!((page.len(), visits), (0, 0), "{status}, descending {descending}, after {outside}");
            }
        }
    }
    // The sort reads every task the equality keeps for its first
    let (_, sorted) = visited(&db, plan("live", json!([is("status", json!("open")), order("m", false), limit(1)])));
    assert_eq!(sorted as usize, holding("open"));
    clean(&db);
}

#[test]
fn an_order_under_two_equalities_is_read_whichever_way_round_they_are_asked() {
    let (_dir, db) = open();
    mixed(&db);
    let asked =
        [[is("owner", json!(1)), is("status", json!("open"))], [is("status", json!("open")), is("owner", json!(1))]];
    for equalities in &asked {
        for descending in [false, true] {
            let steps = |field: &str| {
                let mut steps = equalities.to_vec();
                steps.push(order(field, descending));
                steps
            };
            let (sorted, _) = same(&db, "live", N, |field| json!(steps(field)));
            assert_eq!(sorted.len(), 4, "the open tasks of the first owner");
            let (first, visits) = same(&db, "live", N, |field| {
                let mut steps = steps(field);
                steps.push(limit(3));
                json!(steps)
            });
            assert_eq!((first, visits), (sorted[..3].to_vec(), 3), "{equalities:?}, descending {descending}");
            let (page, visits) = same(&db, "live", N, |field| {
                let mut steps = steps(field);
                steps.extend([after(&sorted[1]), limit(2)]);
                json!(steps)
            });
            assert_eq!((page, visits), (sorted[2..4].to_vec(), 2), "{equalities:?}, descending {descending}");
        }
    }
    // An equality the index doesn't lead with, and one asked twice, are filtered and the rest sorted
    let others = [
        vec![is("owner", json!(1)), is("note", json!("x"))],
        vec![is("status", json!("open")), is("note", json!("x"))],
        vec![is("status", json!("open")), is("n", json!(1))],
        vec![is("status", json!("open")), is("status", json!("open"))],
        vec![is("status", json!(null))],
        vec![is("status", json!("open")), json!({ "step": "has", "field": "owner" })],
    ];
    for filters in others {
        let (_, visits) = same(&db, "live", N, |field| {
            let mut steps = filters.clone();
            steps.extend([order(field, false), limit(1)]);
            json!(steps)
        });
        assert!(visits > 1, "{filters:?}: more than the one it answers is read, {visits}");
    }
}

#[test]
fn a_range_under_an_equality_is_read_from_the_index_on_both() {
    let (_dir, db) = open();
    mixed(&db);
    let open = is("status", json!("open"));
    let between = |field: &str| json!({ "step": "range", "field": field, "atLeast": 1, "below": 3 });
    // The open tasks whose number is 1 or 2.5, in the order they were made: its entries are what is read
    let (found, visits) = same(&db, "live", N, |field| json!([open, between(field)]));
    assert_eq!(found, [id(4), id(6), id(12), id(14), id(20), id(22)]);
    assert_eq!(visits, 6);
    // With one bound, the numbers under the equality end where its strings would begin
    let (from_two, visits) =
        same(&db, "live", N, |field| json!([open, { "step": "range", "field": field, "atLeast": 2 }]));
    assert_eq!(from_two, [id(0), id(4), id(8), id(12), id(16), id(20)]);
    assert_eq!(visits, 6);
    // In the number's order, its first two and a page after one of them
    for descending in [false, true] {
        let steps = |field: &str| vec![open.clone(), between(field), order(field, descending)];
        let (sorted, _) = same(&db, "live", N, |field| json!(steps(field)));
        assert_eq!(sorted.len(), 6);
        let (first, visits) = same(&db, "live", N, |field| {
            let mut steps = steps(field);
            steps.push(limit(2));
            json!(steps)
        });
        assert_eq!((first, visits), (sorted[..2].to_vec(), 2), "descending {descending}");
        let (page, visits) = same(&db, "live", N, |field| {
            let mut steps = steps(field);
            steps.extend([after(&sorted[2]), limit(2)]);
            json!(steps)
        });
        assert_eq!((page, visits), (sorted[3..5].to_vec(), 2), "descending {descending}");
        // A cursor the equality keeps and the range doesn't: 3 is outside it, and so is no number
        for outside in [id(0), id(2)] {
            let (page, visits) = same(&db, "live", N, |field| {
                let mut steps = steps(field);
                steps.extend([after(&outside), limit(2)]);
                json!(steps)
            });
            assert_eq!((page.len(), visits), (0, 0), "descending {descending}, after {outside}");
        }
    }
    // The range before the equality is the same set read the same way, by whichever of them comes last; and among
    // two equalities, from the index on both and the number
    let (swapped, visits) = same(&db, "live", N, |field| json!([between(field), open]));
    assert_eq!((swapped, visits), (found.clone(), 6));
    let first_owner = is("owner", json!(1));
    // The range first, between the two, and last
    for at in 0..3 {
        let (owned, visits) = same(&db, "live", N, |field| {
            let mut asked = vec![first_owner.clone(), open.clone()];
            asked.insert(at, between(field));
            json!(asked)
        });
        assert_eq!((owned, visits), (vec![id(6), id(12)], 2), "the range at {at}");
    }
    // With no index on the two, the equality's own is read whichever of them comes first, and the range filters
    for range_first in [false, true] {
        let (owned, visits) = same(&db, "live", N, |field| {
            if range_first { json!([between(field), first_owner]) } else { json!([first_owner, between(field)]) }
        });
        assert_eq!((owned, visits), (vec![id(6), id(9), id(12)], 8), "the range first: {range_first}");
    }
    // The stage that ends them names the index, and the ones before it read nothing
    let explained =
        read(&db, json!({ "read": "query", "explain": true, "plan": plan("live", json!([between("n"), open])) }));
    let stages = explained.unwrap()["stages"].clone();
    assert_eq!(
        [&stages[1]["visited"], &stages[2]["index"], &stages[2]["visited"], &stages[2]["kept"]],
        [&json!(0), &json!("statusN"), &json!(6), &json!(6)]
    );
    // An equality no index holds with them filters what the others read from theirs
    let noted = |field: &str| json!([between(field), open, is("note", json!("x"))]);
    assert_eq!(same(&db, "live", N, noted), (vec![], 6));
    // Over two types the equality is read from each type's own index, or its rows, and the range filters
    let both = |field: &str| {
        json!({ "scope": "live", "start": { "from": "types", "types": ["Task", "Other"] },
            "steps": [between(field), open], "terminal": { "answer": "ids" } })
    };
    assert_eq!(visited(&db, both("n")), (found.clone(), 24));
    assert_eq!(visited(&db, both("m")).0, found);
    // A second range filters what the first read
    let twice = |field: &str| json!([open, between(field), { "step": "range", "field": field, "above": 1 }]);
    assert_eq!(same(&db, "live", N, twice), (vec![id(4), id(12), id(20)], 6));
    // The trash is left out, or kept, as the scope says
    write(&db, vec![json!({ "op": "trash", "id": id(6) })]);
    assert_eq!(same(&db, "live", N, |field| json!([open, between(field)])).0.len(), 5);
    assert_eq!(same(&db, "withTrashed", N, |field| json!([open, between(field)])), (found.clone(), 6));
    // A plan of the trash walks the trash, one task, where that is fewer than the index's entries, which are of
    // every task: wherever the range stands
    assert_eq!(same(&db, "trashed", N, |field| json!([open, between(field)])), (vec![id(6)], 1));
    assert_eq!(same(&db, "trashed", N, |field| json!([between(field), open])), (vec![id(6)], 1));
    let among = |field: &str| json!([first_owner, between(field), open]);
    assert_eq!(same(&db, "trashed", N, among), (vec![id(6)], 1));

    // A range given no bound is refused before an index is read for a filter after it, whether or not that index
    // has a task for the range to be tested against: no task has this owner, and none holds this role
    let unbounded = |field: &str| json!({ "step": "range", "field": field });
    for last in [is("owner", json!(7)), json!({ "step": "withRole", "role": "lead" })] {
        for first in [vec![], vec![json!({ "step": "has", "field": "note" })], vec![between("n")]] {
            let mut steps = first.clone();
            steps.extend([unbounded("n"), last.clone()]);
            let refused = read(&db, json!({ "read": "query", "plan": plan("live", json!(steps)) })).unwrap_err();
            assert_eq!(refused.code, ErrorCode::BatchInvalid, "{steps:?}");
        }
    }
}

#[test]
fn a_range_read_with_an_equality_is_read_before_an_index_a_later_equality_has() {
    let mut schema = schema_json();
    schema["types"]["Task"]["indexes"]["noteRank"] = json!({ "kind": "value", "fields": ["note", "rank"] });
    let (_dir, db) = open_with(schema);
    mixed(&db);
    // Three tasks of the first owner's eight hold this note, two of them with a rank between the bounds: the index
    // on the note and the rank reads those two, where the owner's own index, the one the plan would else read
    // its filters from, holds eight
    let noted = [(0, 1), (3, 2), (9, 5)];
    write(
        &db,
        noted.iter().flat_map(|(at, rank)| [set(*at, "note", json!("x")), set(*at, "rank", json!(rank))]).collect(),
    );
    let between = json!({ "step": "range", "field": "rank", "atLeast": 1, "atMost": 2 });
    let ranked = json!([is("note", json!("x")), between, is("owner", json!(1))]);
    assert_eq!(visited(&db, plan("live", ranked)), (vec![id(0), id(3)], 2));
    // And with the range first, which has no index of its own
    let ranked = json!([between, is("note", json!("x")), is("owner", json!(1))]);
    assert_eq!(visited(&db, plan("live", ranked)), (vec![id(0), id(3)], 2));
    // A role after them is a filter of those two too, though its own index holds the one task
    write(&db, vec![json!({ "op": "grant", "id": id(0), "role": "lead" })]);
    let led = json!([is("note", json!("x")), between, { "step": "withRole", "role": "lead" }]);
    assert_eq!(visited(&db, plan("live", led)), (vec![id(0)], 2));
}

#[test]
fn a_value_the_index_does_not_order_has_the_order_sorted() {
    let (_dir, db) = open();
    mixed(&db);
    let open = is("status", json!("open"));
    let done = is("status", json!("done"));
    let first = |db: &Database, status: &Json| {
        same(db, "live", N, |field| json!([status, order(field, false), limit(2)])).1 as usize
    };
    // A string under one value of the equality: that value's tasks are sorted, the other's still read in order
    write(&db, vec![set(0, "n", json!("text"))]);
    assert_eq!((first(&db, &open), first(&db, &done)), (holding("open"), 2));
    same(&db, "live", N, |field| json!([open, order(field, true)]));
    clean(&db);
    write(&db, vec![set(0, "n", json!(3))]);
    assert_eq!(first(&db, &open), 2, "a number in its place is read in order again");

    // A list in the ordered field, or in the equality's, is a value the index doesn't order at all
    for (field, value) in [("n", json!([1, 2])), ("status", json!(["open", "done"]))] {
        write(&db, vec![set(1, field, value.clone())]);
        assert_eq!(filed(&db, Prefix::IndexUnordered, "statusN"), [id(1)], "{field}");
        let both = (first(&db, &open), first(&db, &done));
        let listed = usize::from(field == "status");
        assert_eq!(both, (holding("open") + listed, holding("done")), "{field}: both are sorted");
        // And a range under the equality is filtered from the equality's own index
        let ranged = |field: &str| json!([done, { "step": "range", "field": field, "atLeast": 0 }]);
        assert_eq!(same(&db, "live", N, ranged).1 as usize, holding("done"), "{field}");
        clean(&db);
        write(&db, vec![set(1, field, if field == "n" { json!(1) } else { json!("done") })]);
        assert_eq!(first(&db, &done), 2, "{field}: gone, the index is read");
    }

    // The trash alone, and a start of two types, are sorted
    write(&db, vec![json!({ "op": "trash", "id": id(2) }), json!({ "op": "trash", "id": id(4) })]);
    let (trash, visits) = same(&db, "trashed", N, |field| json!([open, order(field, false), limit(1)]));
    assert_eq!((trash.len(), visits as usize), (1, 2), "the trash's two are walked, fewer than the open's entries");
    // As many entries as the trash holds tasks are read from the index: walking the trash would read no fewer
    write(&db, vec![set(20, "status", json!("zed")), set(21, "status", json!("zed"))]);
    let zed = plan("trashed", json!([is("status", json!("zed"))]));
    let explained = read(&db, json!({ "read": "query", "explain": true, "plan": zed })).unwrap();
    let stage = &explained["stages"][1];
    assert_eq!((&stage["index"], &stage["visited"]), (&json!("byStatus"), &json!(2)), "{explained}");
    let two = |field: &str| {
        json!({ "start": { "from": "types", "types": ["Task", "Other"] },
            "steps": [open, order(field, false), limit(1)], "terminal": { "answer": "ids" } })
    };
    let (indexed, visits) = visited(&db, two("n"));
    assert_eq!(indexed, visited(&db, two("m")).0);
    assert!(visits > 1);
}

#[test]
fn an_index_on_several_fields_accounts_for_the_tasks_an_equality_can_name() {
    let (_dir, db) = open();
    let without = |db: &Database, index: &str| filed(db, Prefix::IndexWithout, index);
    let unordered = |db: &Database, index: &str| filed(db, Prefix::IndexUnordered, index);
    let expect = |db: &Database, lacking: &[usize], odd: &[usize], what: &str| {
        assert_eq!(without(db, "statusN"), lacking.iter().map(|at| id(*at)).collect::<Vec<_>>(), "without: {what}");
        assert_eq!(unordered(db, "statusN"), odd.iter().map(|at| id(*at)).collect::<Vec<_>>(), "unordered: {what}");
        clean(db);
    };
    write(
        &db,
        vec![
            task(0, json!({})),
            task(1, json!({ "status": "open" })),
            task(2, json!({ "status": "done", "n": null })),
            task(3, json!({ "status": "open", "n": 5 })),
            task(4, json!({ "n": 5 })),
            task(5, json!({ "status": null })),
            task(6, json!({ "status": "open", "n": [5] })),
            task(7, json!({ "status": { "k": 1 }, "n": 5 })),
            task(8, json!({ "status": "open", "owner": 1 })),
        ],
    );
    // Under each status, by place: no status, or a null one, is under none; a list and a map are not ordered
    expect(&db, &[2, 1, 8], &[6, 7], "as created");
    // The index on three fields files a task once its owner and its status are both there
    assert_eq!(without(&db, "ownerStatusN"), [id(8)]);
    assert!(unordered(&db, "ownerStatusN").iter().eq(&[id(6), id(7)]));

    write(&db, vec![set(1, "n", json!(1))]);
    expect(&db, &[2, 8], &[6, 7], "a number given");
    write(&db, vec![set(1, "n", json!(null))]);
    expect(&db, &[2, 1, 8], &[6, 7], "null for it");
    write(&db, vec![set(1, "status", json!("done"))]);
    expect(&db, &[1, 2, 8], &[6, 7], "under another status");
    write(&db, vec![json!({ "op": "delete", "id": id(1), "field": "status" })]);
    expect(&db, &[2, 8], &[6, 7], "its status removed");
    write(&db, vec![set(0, "status", json!("open"))]);
    expect(&db, &[2, 0, 8], &[6, 7], "a status given to one with no number");
    write(&db, vec![set(4, "status", json!("open"))]);
    expect(&db, &[2, 0, 8], &[6, 7], "a status given to one with a number");
    write(&db, vec![json!({ "op": "delete", "id": id(3), "field": "n" })]);
    expect(&db, &[2, 0, 3, 8], &[6, 7], "a number removed");
    write(&db, vec![set(6, "n", json!(6)), set(7, "status", json!("open"))]);
    expect(&db, &[2, 0, 3, 8], &[], "values the index orders for those it didn't");
    write(&db, vec![set(8, "status", json!([1]))]);
    expect(&db, &[2, 0, 3], &[8], "a list for a status");
    assert_eq!(unordered(&db, "ownerStatusN"), [id(8)]);
    assert!(without(&db, "ownerStatusN").is_empty());

    // The trash moves nothing, and a destroyed task is filed nowhere
    write(&db, vec![json!({ "op": "trash", "id": id(0) })]);
    expect(&db, &[2, 0, 3], &[8], "in the trash");
    for at in [0, 2, 3, 8, 5, 4] {
        write(&db, vec![json!({ "op": "destroy", "id": id(at) })]);
    }
    expect(&db, &[], &[], "destroyed");
    // A batch refused part way keeps none of what it filed
    let refused =
        super::write(&db, vec![task(9, json!({ "status": "open" })), json!({ "op": "destroy", "id": "Task-nobody" })]);
    assert!(refused.is_err());
    expect(&db, &[], &[], "a refused batch");
}

#[test]
fn an_index_on_several_fields_declared_over_tasks_or_built_again_accounts_for_them() {
    let mut bare = schema_json();
    for index in ["statusN", "ownerStatusN"] {
        bare["types"]["Task"]["indexes"].as_object_mut().unwrap().remove(index);
    }
    let (_dir, db) = open_with(bare);
    mixed(&db);
    write(&db, vec![set(1, "n", json!([1]))]);
    assert!(filed(&db, Prefix::IndexWithout, "statusN").is_empty());
    db.set_schema(Schema::from_json(&schema_json()).unwrap()).unwrap();
    db.wait_for_index_builds().unwrap();
    let lacking = filed(&db, Prefix::IndexWithout, "statusN");
    // The tasks with a status and no number, null among them: under done, then under open
    assert_eq!(lacking, [id(11), id(19), id(2), id(10), id(18)]);
    assert_eq!(filed(&db, Prefix::IndexUnordered, "statusN"), [id(1)]);
    clean(&db);
    write(&db, vec![set(1, "n", json!(1))]);
    let open = is("status", json!("open"));
    assert_eq!(same(&db, "live", N, |field| json!([open, order(field, true), limit(3)])).1, 3, "built, it is read");
    for index in [Some(("Task", "statusN")), None] {
        db.rebuild(index).unwrap();
        db.wait_for_index_builds().unwrap();
        assert_eq!(filed(&db, Prefix::IndexWithout, "statusN"), lacking, "{index:?}");
        assert!(filed(&db, Prefix::IndexUnordered, "statusN").is_empty(), "{index:?}");
        clean(&db);
    }
}

/// Six hundred tasks whose `k` is their place: the first hundred done and the rest open, an owner of their own for the
/// last three, with a note on those too, and another owner for the hundred and forty-seven before them
fn many(db: &Database) {
    let tasks = (0..600)
        .map(|at| {
            let mut fields = json!({ "k": at, "status": if at < 100 { "done" } else { "open" } });
            if at >= 597 {
                fields["owner"] = json!(7);
                fields["note"] = json!("late");
            } else if at >= 450 {
                fields["owner"] = json!(9);
            }
            task(at, fields)
        })
        .collect();
    write(db, tasks);
}

#[test]
fn with_no_index_on_both_the_order_is_walked_while_that_is_the_cheaper_way() {
    let (_dir, db) = open();
    many(&db);
    let first = |filters: Vec<Json>, descending: bool, take: usize| {
        same(&db, "live", K, |field| {
            let mut steps = filters.clone();
            steps.extend([order(field, descending), limit(take)]);
            json!(steps)
        })
    };
    // The greatest are open: three are read for three, where sorting reads the five hundred open
    assert_eq!(first(vec![is("status", json!("open"))], true, 3), (vec![id(599), id(598), id(597)], 3));
    // The least hundred are done, so the first three open are a hundred and three in, fewer than the walk reads
    // before it asks
    assert_eq!(first(vec![is("status", json!("open"))], false, 3), (vec![id(100), id(101), id(102)], 103));
    // Three tasks have this owner: so few are read from the filter's index and sorted, and the order isn't walked
    assert_eq!(first(vec![is("owner", json!(7))], false, 2), (vec![id(597), id(598)], 3));
    // A hundred and forty-seven have this one, late by the number: enough for the walk to be tried, and fewer than
    // twice the hundred and twenty-eight it has read when it first asks, so it is given up and they are sorted
    assert_eq!(first(vec![is("owner", json!(9))], false, 2), (vec![id(450), id(451)], 128 + 147));
    // A walk that has all it takes as it comes to ask is done and not given up: from the other end the hundred and
    // twenty-fifth of them is the hundred and twenty-eighth task read
    let last = first(vec![is("owner", json!(9))], true, 125);
    assert_eq!(last, ((472..=596).rev().map(id).collect(), 128));
    // A range the plan starts with reads its own index though an equality after it has one too: the ten tasks in it
    let from = |field: &str| json!([{ "step": "range", "field": field, "atLeast": 590 }, is("status", json!("open"))]);
    assert_eq!(same(&db, "live", K, from).1, 10);
    // A range that isn't the plan's first step is a filter of the index a later equality has, though its own field
    // has one: the five hundred open tasks, where no index would walk the six hundred
    let later = json!([{ "step": "has", "field": "note" }, { "step": "range", "field": "k", "atLeast": 590 }, is("status", json!("open"))]);
    assert_eq!(visited(&db, plan("live", later)), (vec![id(597), id(598), id(599)], 500));
    // From the other end the walk meets them after the last three, and is done before it asks
    assert_eq!(first(vec![is("owner", json!(9))], true, 2), (vec![id(596), id(595)], 5));
    // A filter with no index reads the type, six hundred: fewer than twice what the walk has read at its third
    // asking, at five hundred and twelve, and at neither before it
    let noted = json!({ "step": "has", "field": "note" });
    assert_eq!(first(vec![noted.clone()], false, 2), (vec![id(597), id(598)], 512 + 600));
    // Each of two lists of ids to keep is tested as its own
    let listed = |ids: [usize; 3]| json!({ "step": "within", "ids": ids.map(id) });
    assert_eq!(first(vec![listed([0, 1, 2]), listed([1, 2, 3])], false, 2), (vec![id(1), id(2)], 3));
    // Ids to keep are a filter with no index too: the greater of these two is three hundred in from the greatest
    let two = json!({ "step": "within", "ids": [id(5), id(300)] });
    assert_eq!(first(vec![two.clone()], true, 1), (vec![id(300)], 300));
    assert_eq!(first(vec![two, is("status", json!("done"))], true, 1), (vec![id(5)], 100));
    // A filter of null is no equality an index reads: the tasks with no owner are the first four hundred and fifty,
    // met a hundred and fifty in from the greatest
    assert_eq!(first(vec![is("owner", json!(null))], true, 2), (vec![id(449), id(448)], 152));
    // Several filters: the one whose index they are read from says what reading them would read, the first that has
    // one. The owner's three are read; the five hundred open are enough to walk for, and given up at the walk's
    // second asking
    assert_eq!(first(vec![noted, is("owner", json!(7))], true, 1), (vec![id(599)], 3));
    assert_eq!(first(vec![is("status", json!("open")), is("owner", json!(7))], false, 1).1, 256 + 500);
    assert_eq!(first(vec![is("status", json!("open")), is("owner", json!(9))], true, 1), (vec![id(596)], 4));

    // Without a limit straight after it the set is sorted, as it is after a range, which reads its own index
    let all = same(&db, "live", K, |field| json!([is("owner", json!(7)), order(field, false)]));
    assert_eq!(all, (vec![id(597), id(598), id(599)], 3));
    let ranged = |field: &str| json!([{ "step": "range", "field": if field == "k" { "j" } else { "k" }, "atLeast": 590 }, order(field, true), limit(1)]);
    assert_eq!(same(&db, "live", K, ranged).0, [id(599)]);
    // A step that is no filter of the start's keeps the order after it: the first three hundred made, then sorted
    let some = same(&db, "live", K, |field| json!([limit(300), order(field, true), limit(2)]));
    assert_eq!(some, (vec![id(299), id(298)], 300));
    // A type of exactly as many entities as the walk is tried for is walked: its one note is the sixth by the number
    let small = (0..128).map(|at| {
        let mut fields = json!({ "k": at });
        if at == 5 {
            fields["note"] = json!("here");
        }
        json!({ "op": "create", "type": "Small", "id": format!("Small-{at:03}"), "fields": fields })
    });
    write(&db, small.collect());
    let noted = json!({ "scope": "live", "start": { "from": "types", "types": ["Small"] },
        "steps": [{ "step": "has", "field": "note" }, order("k", false), limit(1)], "terminal": { "answer": "ids" } });
    assert_eq!(visited(&db, noted), (vec!["Small-005".to_owned()], 6));
    // And a role the schema doesn't declare is refused, as the filter itself refuses it
    let undeclared = plan("live", json!([{ "step": "withRole", "role": "nobody" }, order("k", false), limit(1)]));
    assert_eq!(read(&db, json!({ "read": "query", "plan": undeclared })).unwrap_err().code, ErrorCode::UnknownRole);

    // A page of a walked order begins past its cursor, sought in the order's index: three are read for three, where
    // sorting reads the five hundred open
    let page = |filters: Vec<Json>, descending: bool, cursor: usize, take: usize| {
        same(&db, "live", K, |field| {
            let mut steps = filters.clone();
            steps.extend([order(field, descending), after(&id(cursor)), limit(take)]);
            json!(steps)
        })
    };
    let open = is("status", json!("open"));
    assert_eq!(page(vec![open.clone()], true, 597, 3), (vec![id(596), id(595), id(594)], 3));
    assert_eq!(page(vec![open.clone()], false, 300, 2), (vec![id(301), id(302)], 2));
    assert_eq!(page(vec![open.clone()], false, 598, 5), (vec![id(599)], 1));
    // A cursor the filters don't keep, or that is no task, has no page, and nothing is read for it
    assert_eq!(page(vec![open.clone()], false, 50, 2), (vec![], 0));
    assert_eq!(page(vec![open.clone()], false, 999, 2), (vec![], 0));
    // The walk is given up as a first page's is: none with this owner is below its cursor, which the hundred and
    // twenty-eight read before asking don't show, and its hundred and forty-seven are then sorted
    assert_eq!(page(vec![is("owner", json!(9))], true, 450, 2), (vec![], 128 + 147));
    // One is below this cursor: the page given up on is sorted for as a first page is, holding only what it takes
    assert_eq!(page(vec![is("owner", json!(9))], true, 451, 2), (vec![id(450)], 128 + 147));
    assert_eq!(page(vec![is("owner", json!(9))], false, 450, 2), (vec![id(451), id(452)], 2));
    // And it isn't tried for filters that read few, nor without a limit straight after the cursor
    assert_eq!(page(vec![is("owner", json!(7))], false, 597, 2), (vec![id(598), id(599)], 3));
    assert_eq!(page(vec![is("owner", json!(7))], true, 598, 2), (vec![id(597)], 3));
    assert_eq!(page(vec![is("owner", json!(7))], false, 100, 2), (vec![], 0), "a cursor the sorted set doesn't hold");
    let unlimited = same(&db, "live", K, |field| json!([open, order(field, true), after(&id(102))]));
    assert_eq!(unlimited, (vec![id(101), id(100)], 500));
    // The order's stage and its cursor's kept what the limit read for them
    let steps = json!([open, order("k", true), after(&id(597)), limit(3)]);
    let explained = read(&db, json!({ "read": "query", "explain": true, "plan": plan("live", steps) })).unwrap();
    let stage = |at: usize, key: &str| explained["stages"][at][key].clone();
    assert_eq!(
        [stage(2, "stage"), stage(2, "index"), stage(2, "wants")],
        [json!("orderBy"), json!("byK"), json!("status, k")]
    );
    assert_eq!(
        [stage(2, "visited"), stage(2, "kept"), stage(3, "kept"), stage(4, "kept")],
        [json!(3), json!(3), json!(3), json!(3)]
    );

    // A role's holders are a filter like any: two hundred hold this one, the first by the number, so its least is
    // the first the walk reads, and its greatest is past where the walk asks and gives up, two hundred being fewer
    // than twice what it has read
    let stars = (0..200).map(|at| json!({ "op": "grant", "id": id(at), "role": "star" }));
    write(&db, stars.collect());
    let with_role = |role: &str, descending: bool| {
        visited(&db, plan("live", json!([{ "step": "withRole", "role": role }, order("k", descending), limit(1)])))
    };
    assert_eq!(with_role("star", false), (vec![id(0)], 1));
    assert_eq!(with_role("star", true), (vec![id(199)], 128 + 200));
    // Over two types that each have an index on its field, a range the plan starts with reads them: ten tasks,
    // where the role's own index holds two hundred
    let two = json!({ "scope": "live", "start": { "from": "types", "types": ["Task", "Small"] },
        "steps": [{ "step": "range", "field": "k", "atLeast": 590 }, { "step": "withRole", "role": "star" }],
        "terminal": { "answer": "ids" } });
    assert_eq!(visited(&db, two), (vec![], 10));
    // A page under a role is walked from its cursor, and one given up on is sorted
    let role_page = |descending: bool, cursor: usize| {
        let steps =
            json!([{ "step": "withRole", "role": "star" }, order("k", descending), after(&id(cursor)), limit(2)]);
        visited(&db, plan("live", steps))
    };
    assert_eq!(role_page(false, 0), (vec![id(1), id(2)], 2));
    assert_eq!(role_page(true, 199), (vec![id(198), id(197)], 2));
    assert_eq!(role_page(true, 599), (vec![], 0), "no holder");
    // Three hold this one: so few are read from the role's index and sorted
    write(&db, [10, 300, 599].iter().map(|at| json!({ "op": "grant", "id": id(*at), "role": "lead" })).collect());
    assert_eq!(with_role("lead", true), (vec![id(599)], 3));
    // An entry of a task in the trash is read as any is, and counted: with the least three hundred there, the walk
    // asks at its hundred and twenty-eighth entry and gives up at its two hundred and fifty-sixth, the three hundred
    // live being fewer than twice that, having handed on none
    write(&db, (0..300).map(|at| json!({ "op": "trash", "id": id(at) })).collect());
    assert_eq!(first(vec![json!({ "step": "has", "field": "note" })], false, 1), (vec![id(597)], 256 + 300));
    // A plan of both reads the trash's tasks to sort them too: six hundred, which the walk has read half of at its
    // third asking and not at its second
    let both =
        same(&db, "withTrashed", K, |field| json!([{ "step": "has", "field": "note" }, order(field, false), limit(1)]));
    assert_eq!(both, (vec![id(597)], 512 + 600));
    // A plan of the trash walks the trash, three hundred tasks, where an index would read more entries than that,
    // which are of every task: this owner's three and the hundred done are read from their index, the five hundred
    // open are not
    let trashed = |filter: Json| visited(&db, plan("trashed", json!([filter]))).1;
    let from_trash =
        (trashed(is("owner", json!(7))), trashed(is("status", json!("done"))), trashed(is("status", json!("open"))));
    assert_eq!(from_trash, (3, 100, 300));
    // A role the schema no longer declares is refused though tasks still hold it, as its filter refuses it
    let mut fewer = schema_json();
    fewer["roles"].as_object_mut().unwrap().remove("star");
    db.set_schema(Schema::from_json(&fewer).unwrap()).unwrap();
    let dropped = plan("live", json!([{ "step": "withRole", "role": "star" }, order("k", false), limit(1)]));
    assert_eq!(read(&db, json!({ "read": "query", "plan": dropped })).unwrap_err().code, ErrorCode::UnknownRole);
}

#[test]
fn a_tracked_walk_is_touched_by_what_would_change_its_page_and_not_by_what_it_asked_of_the_filters() {
    let (_dir, db) = open();
    many(&db);
    let track = |steps: Json, token: u32| {
        let query = json!({ "read": "query", "plan": plan("live", steps) });
        let reads = Reads { reads: vec![serde_json::from_value::<ReadOp>(query).unwrap()], track: Some(token) };
        db.read(&reads).unwrap();
    };
    let touched = |ops: Vec<Json>| db.write(&super::batch(ops), false).unwrap().1.touched;
    // Walked with no index for its filter, which asks how many tasks there are; and with one, which asks how many
    // entries the filter's index holds: the least two with no note, and the greatest two that are open
    track(json!([{ "step": "lacks", "field": "note" }, order("k", false), limit(2)]), 1);
    track(json!([is("status", json!("open")), order("k", true), limit(2)]), 2);
    // A task past both pages moves the count the first asked of, and one of the least open tasks made done moves
    // the entries the second counted: neither is what a page holds
    assert!(touched(vec![task(700, json!({ "k": 300, "status": "open" }))]).is_empty());
    assert!(touched(vec![set(100, "status", json!("done"))]).is_empty());
    // A task below the least, and one above the greatest
    assert_eq!(touched(vec![task(701, json!({ "k": -1, "status": "done" }))]), [1]);
    assert_eq!(touched(vec![task(702, json!({ "k": 1000, "status": "open" }))]), [2]);
}

#[test]
fn explain_names_the_index_that_would_read_a_sorted_order() {
    let (_dir, db) = open();
    many(&db);
    let ordering = |steps: Json| {
        let answer = read(&db, json!({ "read": "query", "explain": true, "plan": plan("live", steps) })).unwrap();
        let stages = answer["stages"].as_array().unwrap();
        // Each stage is whole: one that says it holds fewer keys than it writes leaves its last outside it
        assert!(stages.iter().all(|stage| stage["kept"].is_number() && stage["ms"].is_number()), "{answer}");
        let stage = stages.iter().find(|stage| stage["stage"] == "orderBy").unwrap();
        (stage["index"].clone(), stage["wants"].clone(), stage["visited"].clone())
    };
    let open = is("status", json!("open"));
    // Walked, with the filter tested: an index on both would read the three alone
    assert_eq!(ordering(json!([open, order("k", false), limit(3)])), (json!("byK"), json!("status, k"), json!(103)));
    // Sorted, with or without a limit, and with a range of the field
    assert_eq!(ordering(json!([open, order("rank", false), limit(3)])).1, "status, rank");
    // No index is named that the schema would refuse: one on the short code, or on a field the type doesn't declare
    // A time the engine writes is one an index may name
    for time in ["createdAt", "updatedAt"] {
        let wanted = ordering(json!([is(time, json!(5)), order("rank", false), limit(3)])).1;
        assert_eq!(wanted, json!(format!("{time}, rank")));
    }
    for refused in ["shortCode", "nowhere"] {
        let wanted = ordering(json!([is(refused, json!("x")), order("rank", false), limit(3)])).1;
        assert_eq!(wanted, Json::Null, "{refused}");
    }
    assert_eq!(ordering(json!([order("rank", false)])).1, "rank");
    let ranged =
        json!([open, { "step": "range", "field": "rank", "atLeast": 1 }, is("owner", json!(7)), order("rank", true)]);
    assert_eq!(ordering(ranged).1, "status, owner, rank");
    assert_eq!(ordering(json!([open, order("createdAt", false)])).1, "status, createdAt");
    assert_eq!(ordering(json!([is("owner", json!(7)), order("updatedAt", true)])).1, "owner, updatedAt");
    // An equality asked twice, or of the ordered field itself, is no index's fields
    assert_eq!(ordering(json!([open, open, order("rank", false)])).1, Json::Null);
    assert_eq!(ordering(json!([is("rank", json!(1)), order("rank", false)])).1, Json::Null);
    // An index that is there wants nothing, whether or not it is what the order was read from
    assert_eq!(ordering(json!([open, order("n", false), limit(3)])), (json!("statusN"), Json::Null, json!(3)));
    assert_eq!(ordering(json!([order("k", false), limit(3)])).1, Json::Null);
    // Nor does an index that holds a string where its field is declared a number, which a load may bring: its
    // order is sorted, and it is not wanted again
    let odd = crate::engine::maintenance::Load::Entity {
        id: "Task-odd".into(),
        fields: [("owner".to_owned(), rmpv::Value::from(7)), ("rank".to_owned(), rmpv::Value::from("text"))].into(),
        trashed: false,
    };
    db.bulk_load(&[odd]).unwrap();
    let ranked = ordering(json!([is("owner", json!(7)), order("rank", false), limit(2)]));
    assert_eq!(ranked, (Json::Null, Json::Null, json!(4)));
    // And none is named where an index wouldn't have the order: a string's is the collation's, an `any` field holds
    // what it holds, and a filter that is no equality is no part of an index
    assert_eq!(ordering(json!([open, order("note", false)])).1, Json::Null);
    assert_eq!(ordering(json!([open, order("m", false)])).1, Json::Null);
    assert_eq!(ordering(json!([{ "step": "has", "field": "note" }, order("rank", false)])).1, Json::Null);
    let trash =
        read(&db, json!({ "read": "query", "explain": true, "plan": plan("trashed", json!([order("rank", false)])) }));
    assert_eq!(trash.unwrap()["stages"][1].get("wants"), None);
}
