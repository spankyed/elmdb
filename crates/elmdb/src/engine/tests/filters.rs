//! A type start's leading filters against a model of them: whichever index the plan reads them from
//! (`reads.rs`, `Query::access`), in whatever order they are written, the answer is the entities the filters keep in
//! creation order, a tracked read is touched by the write that changes it, and the index read is the one holding
//! the fewest entries.

use std::collections::BTreeSet;

use serde_json::{Value as Json, json};

use crate::engine::plan::{ReadOp, Reads};
use crate::schema::Schema;
use crate::{Database, OpenOptions};

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
    fn chance(&mut self, percent: u64) -> bool {
        self.below(100) < percent
    }
}

fn schema() -> Schema {
    let fields = json!({
        "a": { "type": "any", "optional": true },
        "b": { "type": "string", "optional": true },
        "n": { "type": "number", "optional": true },
        "m": { "type": "any", "optional": true }
    });
    Schema::from_json(&json!({
        "$schema": crate::schema::SCHEMA_LANGUAGE,
        "roles": { "r1": {}, "r2": {} },
        "types": {
            "A": { "fields": fields, "indexes": {
                "byA": { "kind": "value", "fields": ["a"] },
                "byB": { "kind": "value", "fields": ["b"] },
                "byN": { "kind": "value", "fields": ["n"] },
                "byM": { "kind": "value", "fields": ["m"] },
                "aN": { "kind": "value", "fields": ["a", "n"] },
                "abN": { "kind": "value", "fields": ["a", "b", "n"] }
            } },
            "B": { "fields": fields, "indexes": {
                "byA": { "kind": "value", "fields": ["a"] },
                "byN": { "kind": "value", "fields": ["n"] }
            } },
            "C": { "fields": fields }
        }
    }))
    .unwrap()
}

#[derive(Clone, Debug)]
struct Ent {
    id: String,
    ty: &'static str,
    a: Option<Json>,
    b: Option<String>,
    n: Option<f64>,
    m: Option<Json>,
    roles: BTreeSet<&'static str>,
    trashed: bool,
    gone: bool,
}

impl Ent {
    fn field(&self, name: &str) -> Option<Json> {
        match name {
            "a" => self.a.clone(),
            "b" => self.b.clone().map(Json::from),
            "n" => self.n.map(|n| json!(n)),
            "m" => self.m.clone(),
            _ => unreachable!(),
        }
    }
    fn create(&self) -> Json {
        let mut fields = json!({});
        for name in ["a", "b", "n", "m"] {
            if let Some(value) = self.field(name) {
                fields[name] = value;
            }
        }
        json!({ "op": "create", "type": self.ty, "id": self.id, "fields": fields })
    }
}

fn pick_a(rng: &mut Rng) -> u64 {
    match rng.below(1000) {
        0..500 => 0,
        500..750 => 1,
        750..880 => 2,
        880..960 => 3,
        960..995 => 4,
        _ => 5,
    }
}

fn gen_a(rng: &mut Rng) -> Option<Json> {
    match rng.below(100) {
        0..10 => None,
        10..14 => Some(json!([pick_a(rng), pick_a(rng)])),
        _ => Some(json!(pick_a(rng))),
    }
}

fn gen_b(rng: &mut Rng) -> Option<String> {
    match rng.below(100) {
        0..20 => None,
        20..65 => Some("x".into()),
        65..95 => Some("y".into()),
        _ => Some("z".into()),
    }
}

fn gen_n(rng: &mut Rng) -> Option<f64> {
    if rng.chance(15) {
        return None;
    }
    Some(rng.below(5000) as f64 + if rng.chance(10) { 0.5 } else { 0.0 })
}

fn gen_m(rng: &mut Rng, lists: bool) -> Option<Json> {
    match rng.below(100) {
        0..10 => None,
        10..15 if lists => Some(json!([rng.below(100), rng.below(100)])),
        15..20 => Some(json!("s")),
        _ => Some(json!(rng.below(100))),
    }
}

fn gen_ent(rng: &mut Rng, at: usize, lists: bool) -> Ent {
    let ty = if at % 9 == 8 {
        "B"
    } else if at % 61 == 60 {
        "C"
    } else {
        "A"
    };
    let mut roles = BTreeSet::new();
    if rng.chance(40) {
        roles.insert("r1");
    }
    if rng.chance(2) {
        roles.insert("r2");
    }
    Ent {
        id: format!("{ty}-{at:05}"),
        ty,
        a: gen_a(rng),
        b: gen_b(rng),
        n: gen_n(rng),
        m: gen_m(rng, lists),
        roles,
        trashed: false,
        gone: false,
    }
}

#[derive(Clone, Debug)]
enum F {
    Where(&'static str, Json),
    Range(&'static str, Option<(f64, bool)>, Option<(f64, bool)>),
    Role(&'static str),
    Has(&'static str),
    Lacks(&'static str),
    OfType(&'static str),
    Within(Vec<String>),
}

fn same(x: &Json, y: &Json) -> bool {
    match (x.as_f64(), y.as_f64()) {
        (Some(x), Some(y)) => x == y,
        _ => x == y,
    }
}

impl F {
    fn json(&self) -> Json {
        match self {
            F::Where(field, value) => json!({ "step": "where", "field": field, "value": value }),
            F::Range(field, low, high) => {
                let mut step = json!({ "step": "range", "field": field });
                if let Some((bound, within)) = low {
                    step[if *within { "atLeast" } else { "above" }] = json!(bound);
                }
                if let Some((bound, within)) = high {
                    step[if *within { "atMost" } else { "below" }] = json!(bound);
                }
                step
            }
            F::Role(role) => json!({ "step": "withRole", "role": role }),
            F::Has(field) => json!({ "step": "has", "field": field }),
            F::Lacks(field) => json!({ "step": "lacks", "field": field }),
            F::OfType(t) => json!({ "step": "ofType", "type": t }),
            F::Within(ids) => json!({ "step": "within", "ids": ids }),
        }
    }

    fn keeps(&self, e: &Ent) -> bool {
        match self {
            F::Where(field, value) => match e.field(field) {
                None => value.is_null(),
                Some(Json::Array(items)) => items.iter().any(|item| same(item, value)),
                Some(stored) => same(&stored, value),
            },
            F::Range(field, low, high) => {
                let within = |n: f64| {
                    low.is_none_or(|(bound, within)| n > bound || (within && n == bound))
                        && high.is_none_or(|(bound, within)| n < bound || (within && n == bound))
                };
                match e.field(field) {
                    Some(Json::Array(items)) => items.iter().any(|item| item.as_f64().is_some_and(within)),
                    Some(stored) => stored.as_f64().is_some_and(within),
                    None => false,
                }
            }
            F::Role(role) => e.roles.contains(role),
            F::Has(field) => e.field(field).is_some(),
            F::Lacks(field) => e.field(field).is_none(),
            F::OfType(t) => e.ty == *t,
            F::Within(ids) => ids.contains(&e.id),
        }
    }
}

fn gen_filter(rng: &mut Rng, ents: &[Ent]) -> F {
    match rng.below(13) {
        0..3 => F::Where("a", if rng.chance(8) { Json::Null } else { json!(rng.below(6)) }),
        3..5 => F::Where("b", json!(["x", "y", "z", "w"][rng.below(4) as usize])),
        5..8 => {
            let width = [5.0, 40.0, 400.0, 2500.0, 6000.0][rng.below(5) as usize];
            let low = rng.below(5000) as f64;
            let (closed_low, closed_high) = (rng.chance(50), rng.chance(50));
            match rng.below(4) {
                0 => F::Range("n", Some((low, closed_low)), None),
                1 => F::Range("n", None, Some((low, closed_high))),
                _ => F::Range("n", Some((low - width / 2.0, closed_low)), Some((low + width / 2.0, closed_high))),
            }
        }
        8 => {
            let low = rng.below(100) as f64;
            let width = [3.0, 30.0, 200.0][rng.below(3) as usize];
            F::Range("m", Some((low, rng.chance(50))), Some((low + width, rng.chance(50))))
        }
        9..11 => F::Role(if rng.chance(60) { "r2" } else { "r1" }),
        11 => match rng.below(4) {
            0 => F::Has("b"),
            1 => F::Lacks("b"),
            2 => F::Has("a"),
            _ => F::Lacks("n"),
        },
        _ => {
            if rng.chance(40) {
                F::OfType(["A", "B", "C"][rng.below(3) as usize])
            } else {
                let ids = (0..60).map(|_| ents[rng.below(ents.len() as u64) as usize].id.clone()).collect();
                F::Within(ids)
            }
        }
    }
}

fn expected(ents: &[Ent], types: &[&str], scope: &str, filters: &[F]) -> Vec<String> {
    let mut out = Vec::new();
    for t in types {
        for e in ents {
            let reached = match scope {
                "live" => !e.trashed,
                "trashed" => e.trashed,
                _ => true,
            };
            if !e.gone && e.ty == *t && reached && filters.iter().all(|f| f.keeps(e)) {
                out.push(e.id.clone());
            }
        }
    }
    out
}

type Reader<'a> = dyn FnMut(&Reads) -> crate::Result<Vec<u8>> + 'a;

fn answer(read: &mut Reader<'_>, plan: Json, track: Option<u32>) -> Json {
    let op: ReadOp = serde_json::from_value(json!({ "read": "query", "plan": plan })).unwrap();
    let bytes = read(&Reads { reads: vec![op], track }).unwrap();
    let mut answers = serde_json::to_value(rmpv::decode::read_value(&mut &bytes[..]).unwrap()).unwrap();
    answers[0].take()
}

fn plan(types: &[&str], scope: &str, steps: Vec<Json>, terminal: &str) -> Json {
    json!({ "scope": scope, "start": { "from": "types", "types": types }, "steps": steps,
        "terminal": { "answer": terminal } })
}

fn orders<T: Clone>(items: &[T]) -> Vec<Vec<T>> {
    if items.len() <= 1 {
        return vec![items.to_vec()];
    }
    let mut out = Vec::new();
    for at in 0..items.len() {
        let mut rest = items.to_vec();
        let first = rest.remove(at);
        for mut order in orders(&rest) {
            order.insert(0, first.clone());
            out.push(order);
        }
    }
    out
}

fn open() -> (tempfile::TempDir, Database) {
    let dir = tempfile::tempdir().unwrap();
    let db =
        Database::open(dir.path(), OpenOptions { schema: Some(schema()), syncs: false, ..Default::default() }).unwrap();
    (dir, db)
}

/// The ops that make `ents` as they are
fn ops_for(ents: &[Ent]) -> Vec<Json> {
    let mut ops: Vec<Json> = ents.iter().map(Ent::create).collect();
    for e in ents {
        for role in &e.roles {
            ops.push(json!({ "op": "grant", "id": e.id, "role": role }));
        }
        if e.trashed {
            ops.push(json!({ "op": "trash", "id": e.id }));
        }
    }
    ops
}

const STARTS: [&[&str]; 7] = [&["A"], &["A"], &["A", "B"], &["B", "A"], &["A", "B", "C"], &["A", "A"], &["B"]];

fn differential(seed: u64, count: usize, lists: bool, trash_percent: u64, in_unit: bool, rounds: usize) {
    let mut rng = Rng(seed);
    let mut ents: Vec<Ent> = (0..count).map(|at| gen_ent(&mut rng, at, lists)).collect();
    for e in &mut ents {
        e.trashed = rng.chance(trash_percent);
    }
    let (_dir, db) = open();
    let committed = if in_unit { count / 2 } else { count };
    for some in ents[..committed].chunks(400) {
        super::write(&db, ops_for(some)).unwrap();
    }
    let mut unit = in_unit.then(|| db.begin().unwrap());
    if let Some(unit) = &mut unit {
        for some in ents[committed..].chunks(400) {
            unit.write(&super::batch(ops_for(some))).unwrap();
        }
        // And what the unit changes of what was committed: values moved between indexes' entries
        let mut ops = Vec::new();
        for _ in 0..committed / 3 {
            let at = rng.below(committed as u64) as usize;
            let e = &mut ents[at];
            match rng.below(4) {
                0 => {
                    e.a = Some(json!(pick_a(&mut rng)));
                    ops.push(json!({ "op": "set", "id": e.id, "fields": { "a": e.a } }));
                }
                1 => {
                    e.n = Some(rng.below(5000) as f64);
                    ops.push(json!({ "op": "set", "id": e.id, "fields": { "n": e.n } }));
                }
                2 => {
                    if e.roles.insert("r2") {
                        ops.push(json!({ "op": "grant", "id": e.id, "role": "r2" }));
                    }
                }
                _ => {
                    if !e.trashed {
                        e.trashed = true;
                        ops.push(json!({ "op": "trash", "id": e.id }));
                    }
                }
            }
        }
        unit.write(&super::batch(ops)).unwrap();
    }
    let mut read = |reads: &Reads| match &mut unit {
        Some(unit) => unit.read(reads),
        None => db.read(reads),
    };
    let mut checked = 0;
    for round in 0..rounds {
        let filters: Vec<F> = (0..2 + rng.below(3)).map(|_| gen_filter(&mut rng, &ents)).collect();
        let types = STARTS[rng.below(STARTS.len() as u64) as usize];
        let scope = ["live", "live", "withTrashed", "trashed"][rng.below(4) as usize];
        let want = expected(&ents, types, scope, &filters);
        let all = orders(&filters);
        for (nth, order) in all.iter().enumerate() {
            let steps: Vec<Json> = order.iter().map(F::json).collect();
            let context = || format!("seed {seed} round {round} {types:?} {scope} {}", json!(steps));
            let got = answer(&mut read, plan(types, scope, steps.clone(), "ids"), None);
            let got: Vec<String> =
                got["ids"].as_array().unwrap().iter().map(|id| id.as_str().unwrap().into()).collect();
            assert_eq!(got, want, "{}", context());
            checked += 1;
            if nth != 0 && nth != all.len() - 1 && nth != all.len() / 2 {
                continue;
            }
            let first = answer(&mut read, plan(types, scope, steps.clone(), "first"), None);
            assert_eq!(first["id"], want.first().map_or(Json::Null, |id| json!(id)), "first: {}", context());
            // The first three in a number's order, either way: ties in the set's order, those without one last
            // ascending and first descending
            for descending in [false, true] {
                let mut ordered = steps.clone();
                ordered.push(json!({ "step": "orderBy", "field": "n", "descending": descending }));
                ordered.push(json!({ "step": "limit", "n": 3 }));
                let got = answer(&mut read, plan(types, scope, ordered, "ids"), None);
                let number = |id: &String| ents.iter().find(|e| e.id == *id).unwrap().n;
                let mut sorted: Vec<&String> = want.iter().collect();
                sorted.sort_by(|x, y| match (number(x), number(y)) {
                    (Some(x), Some(y)) if descending => y.partial_cmp(&x).unwrap(),
                    (Some(x), Some(y)) => x.partial_cmp(&y).unwrap(),
                    (None, None) => std::cmp::Ordering::Equal,
                    (None, Some(_)) if descending => std::cmp::Ordering::Less,
                    (None, Some(_)) => std::cmp::Ordering::Greater,
                    (Some(_), None) if descending => std::cmp::Ordering::Greater,
                    (Some(_), None) => std::cmp::Ordering::Less,
                });
                let sorted: Vec<Json> = sorted.into_iter().take(3).map(|id| json!(id)).collect();
                assert_eq!(got["ids"], json!(sorted), "ordered {descending}: {}", context());
            }
            let exists = answer(&mut read, plan(types, scope, steps.clone(), "exists"), None);
            assert_eq!(exists["value"], json!(!want.is_empty()), "exists: {}", context());
            // Pages after cursors of the set, where no type is named twice, and after one outside it
            let repeats = types.len() == 2 && types[0] == types[1];
            let mut cursors: Vec<String> = Vec::new();
            if !repeats && !want.is_empty() {
                cursors.extend([want[0].clone(), want[want.len() / 2].clone(), want[want.len() - 1].clone()]);
            }
            let outside = &ents[rng.below(ents.len() as u64) as usize].id;
            if !want.contains(outside) {
                cursors.push(outside.clone());
            }
            for cursor in cursors {
                let mut paged = steps.clone();
                paged.push(json!({ "step": "after", "id": cursor }));
                paged.push(json!({ "step": "limit", "n": 3 }));
                let got = answer(&mut read, plan(types, scope, paged, "ids"), None);
                let page: Vec<Json> = match want.iter().position(|id| *id == cursor) {
                    Some(at) => want[at + 1..].iter().take(3).map(|id| json!(id)).collect(),
                    None => Vec::new(),
                };
                assert_eq!(got["ids"], json!(page), "after {cursor}: {}", context());
            }
        }
    }
    assert!(checked >= rounds * 2, "{checked} plans checked");
}

#[test]
fn the_filters_answer_as_the_model_does_in_every_order_over_many_entities() {
    differential(1, 2600, false, 1, false, 30);
}

#[test]
fn the_filters_answer_as_the_model_does_in_every_order_where_fields_hold_lists() {
    differential(2, 320, true, 10, false, 80);
}

#[test]
fn the_filters_answer_as_the_model_does_in_every_order_where_most_are_trashed() {
    differential(3, 2600, false, 60, false, 25);
}

#[test]
fn the_filters_answer_as_the_model_does_in_every_order_inside_a_unit() {
    differential(4, 2400, false, 3, true, 25);
}

#[test]
fn the_filters_answer_as_the_model_does_in_every_order() {
    differential(5, 900, false, 4, false, 60);
}

/// A tracked read whose answer a commit changed is among those the commit touched
fn tracked(seed: u64, count: usize, writes: usize) {
    let mut rng = Rng(seed);
    let mut ents: Vec<Ent> = (0..count).map(|at| gen_ent(&mut rng, at, false)).collect();
    for e in &mut ents {
        e.trashed = rng.chance(5);
    }
    let (_dir, db) = open();
    for some in ents.chunks(400) {
        super::write(&db, ops_for(some)).unwrap();
    }
    let mut plans: Vec<Json> = Vec::new();
    for _ in 0..40 {
        let filters: Vec<F> = (0..1 + rng.below(3)).map(|_| gen_filter(&mut rng, &ents)).collect();
        let types = STARTS[rng.below(5) as usize];
        let scope = ["live", "live", "withTrashed", "trashed"][rng.below(4) as usize];
        let mut steps: Vec<Json> = filters.iter().map(F::json).collect();
        let terminal = match rng.below(5) {
            0 => "first",
            1 => "exists",
            2 => {
                steps.push(json!({ "step": "limit", "n": 2 }));
                "ids"
            }
            3 => {
                let want = expected(&ents, types, scope, &filters);
                if let Some(cursor) = want.get(want.len() / 3) {
                    steps.push(json!({ "step": "after", "id": cursor }));
                    steps.push(json!({ "step": "limit", "n": 2 }));
                }
                "ids"
            }
            _ => "ids",
        };
        plans.push(plan(types, scope, steps, terminal));
    }
    let mut read = |reads: &Reads| db.read(reads);
    let mut changed = 0;
    let mut answers: Vec<Json> = Vec::new();
    for round in 0..writes {
        answers.clear();
        for (token, plan) in plans.iter().enumerate() {
            db.untrack(token as u32);
            answers.push(answer(&mut read, plan.clone(), Some(token as u32)));
        }
        let at = rng.below(ents.len() as u64) as usize;
        let op = if ents[at].gone || rng.chance(6) {
            let made = gen_ent(&mut rng, ents.len(), false);
            let ops = ops_for(std::slice::from_ref(&made));
            ents.push(made);
            ops
        } else {
            let e = &mut ents[at];
            vec![match rng.below(12) {
                0..3 => {
                    e.a = gen_a(&mut rng).or(Some(json!(5)));
                    json!({ "op": "set", "id": e.id, "fields": { "a": e.a } })
                }
                3 => {
                    if e.a.is_some() {
                        e.a = None;
                        json!({ "op": "delete", "id": e.id, "field": "a" })
                    } else {
                        e.a = Some(json!(4));
                        json!({ "op": "set", "id": e.id, "fields": { "a": 4 } })
                    }
                }
                4..6 => {
                    e.n = Some(rng.below(5000) as f64);
                    json!({ "op": "set", "id": e.id, "fields": { "n": e.n } })
                }
                6 => {
                    e.b = gen_b(&mut rng).or(Some("z".into()));
                    json!({ "op": "set", "id": e.id, "fields": { "b": e.b } })
                }
                7..9 => {
                    let role = if rng.chance(70) { "r2" } else { "r1" };
                    if e.roles.remove(role) {
                        json!({ "op": "revoke", "id": e.id, "role": role })
                    } else {
                        e.roles.insert(role);
                        json!({ "op": "grant", "id": e.id, "role": role })
                    }
                }
                9..11 => {
                    e.trashed = !e.trashed;
                    json!({ "op": if e.trashed { "trash" } else { "restore" }, "id": e.id })
                }
                _ => {
                    e.m = Some(json!(rng.below(100)));
                    json!({ "op": "set", "id": e.id, "fields": { "m": e.m } })
                }
            }]
        };
        let touched = db.write(&super::batch(op.clone()), false).unwrap().1.touched;
        for (token, plan) in plans.iter().enumerate() {
            let now = answer(&mut read, plan.clone(), None);
            if now != answers[token] {
                changed += 1;
                assert!(
                    touched.contains(&(token as u32)),
                    "seed {seed} round {round}: {} changed {plan} from {} to {now} and touched {touched:?}",
                    json!(op),
                    answers[token]
                );
            }
        }
    }
    assert!(changed > 0, "no write changed a tracked answer");
}

#[test]
fn a_tracked_read_of_filters_is_touched_by_each_write_that_changes_it() {
    tracked(11, 500, 120);
}

#[test]
fn a_tracked_read_of_filters_over_many_entities_is_touched_by_each_write_that_changes_it() {
    tracked(12, 2600, 40);
}

/// How many ids a plan answered, and how many entities it visited for them
fn visits(db: &Database, plan: Json) -> (usize, u64) {
    let op: ReadOp = serde_json::from_value(json!({ "read": "query", "visits": true, "plan": plan })).unwrap();
    let bytes = db.read(&Reads { reads: vec![op], track: None }).unwrap();
    let answers = serde_json::to_value(rmpv::decode::read_value(&mut &bytes[..]).unwrap()).unwrap();
    (answers[0]["answer"]["ids"].as_array().unwrap().len(), answers[0]["visited"].as_u64().unwrap())
}

/// A database of `count` entities, alternately an `A` and a `B`, each with the fields `fields` gives its place and
/// its type, and the role `r1`
fn alternating(count: usize, fields: impl Fn(usize, &str) -> Json) -> (tempfile::TempDir, Database) {
    let (dir, db) = open();
    let mut ops = Vec::new();
    for at in 0..count {
        let ty = if at % 2 == 0 { "A" } else { "B" };
        let id = format!("{ty}-{at:05}");
        ops.push(json!({ "op": "create", "type": ty, "id": id, "fields": fields(at, ty) }));
        ops.push(json!({ "op": "grant", "id": id, "role": "r1" }));
    }
    for some in ops.chunks(800) {
        super::write(&db, some.to_vec()).unwrap();
    }
    (dir, db)
}

#[test]
fn an_index_counted_to_its_end_in_each_start_type_is_read_though_its_entries_over_them_pass_the_count() {
    // 700 entries for the value in each type, 1,400 over both, and 2,000 a type for the role
    let (_dir, db) = alternating(4000, |at, _| json!({ "a": if at < 1400 { 1 } else { 0 } }));
    let role = json!({ "step": "withRole", "role": "r1" });
    let one = json!({ "step": "where", "field": "a", "value": 1 });
    for (types, read) in [(vec!["A", "B"], 1400), (vec!["A"], 700), (vec!["A", "A"], 1400)] {
        for steps in [vec![role.clone(), one.clone()], vec![one.clone(), role.clone()]] {
            assert_eq!(
                visits(&db, plan(&types, "live", steps.clone(), "ids")),
                (read, read as u64),
                "{types:?} {steps:?}"
            );
        }
    }
}

#[test]
fn the_fewest_is_chosen_over_the_start_types_together_and_not_at_the_first_round_an_index_ends_in() {
    // The role's entries end in the first round in each type, 15 and 15. The value's are none in `A` and 20 in
    // `B`, past that round's count there: fewer over both, which the next round learns
    let (_dir, db) = open();
    let mut ops = Vec::new();
    for at in 0..400 {
        let ty = if at % 2 == 0 { "A" } else { "B" };
        let id = format!("{ty}-{at:05}");
        let a = if ty == "B" && at < 40 { 1 } else { 0 };
        ops.push(json!({ "op": "create", "type": ty, "id": id, "fields": { "a": a } }));
        if at < 30 {
            ops.push(json!({ "op": "grant", "id": id, "role": "r1" }));
        }
    }
    super::write(&db, ops).unwrap();
    let role = json!({ "step": "withRole", "role": "r1" });
    let one = json!({ "step": "where", "field": "a", "value": 1 });
    for steps in [vec![role.clone(), one.clone()], vec![one.clone(), role.clone()]] {
        // The 15 of `B` with the role are among its first 20, which hold the value
        assert_eq!(visits(&db, plan(&["A", "B"], "live", steps.clone(), "ids")), (15, 20), "{steps:?}");
    }
}

#[test]
fn an_index_counted_to_its_end_is_read_after_the_last_count_whatever_one_still_being_counted_holds() {
    // The value's entries are 1,000 in each type, counted to their end only in the last round. The role's are none
    // in `A` and every one of `B`'s 1,500: never counted to their end, and the count of them, 1,024, is under the
    // value's 2,000, so only the last round settles on the value's
    let (_dir, db) = open();
    let mut ops = Vec::new();
    for at in 0..3000 {
        let ty = if at % 2 == 0 { "A" } else { "B" };
        let id = format!("{ty}-{at:05}");
        ops.push(json!({ "op": "create", "type": ty, "id": id, "fields": { "a": if at < 2000 { 1 } else { 0 } } }));
        if ty == "B" {
            ops.push(json!({ "op": "grant", "id": id, "role": "r1" }));
        }
    }
    for some in ops.chunks(800) {
        super::write(&db, some.to_vec()).unwrap();
    }
    let role = json!({ "step": "withRole", "role": "r1" });
    let one = json!({ "step": "where", "field": "a", "value": 1 });
    for steps in [vec![role.clone(), one.clone()], vec![one.clone(), role.clone()]] {
        assert_eq!(visits(&db, plan(&["A", "B"], "live", steps.clone(), "ids")), (1000, 2000), "{steps:?}");
    }
}

#[test]
fn indexes_holding_as_many_over_the_start_types_are_told_apart_by_which_is_written_first() {
    // Sixteen entries each: the value's all in `A`, as many as the first count goes to there, so not yet known to
    // be all of them when the role's, eight in each type, are
    let (_dir, db) = open();
    let mut ops = Vec::new();
    for at in 0..60 {
        let ty = if at < 40 { "A" } else { "B" };
        let id = format!("{ty}-{at:05}");
        ops.push(json!({ "op": "create", "type": ty, "id": id, "fields": { "a": if at < 16 { 1 } else { 0 } } }));
        if at < 8 || (40..48).contains(&at) {
            ops.push(json!({ "op": "grant", "id": id, "role": "r1" }));
        }
    }
    super::write(&db, ops).unwrap();
    let role = json!({ "step": "withRole", "role": "r1" });
    let one = json!({ "step": "where", "field": "a", "value": 1 });
    // The stage of an explained read that read an index
    let reading = |steps: Vec<Json>| {
        let op = json!({ "read": "query", "explain": true, "plan": plan(&["A", "B"], "live", steps, "ids") });
        let reads = Reads { reads: vec![serde_json::from_value::<ReadOp>(op).unwrap()], track: None };
        let answers = serde_json::to_value(rmpv::decode::read_value(&mut &db.read(&reads).unwrap()[..]).unwrap());
        let explained = answers.unwrap()[0].take();
        let stages: Vec<Json> = explained["stages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|stage| stage.get("index").is_some())
            .cloned()
            .collect();
        assert_eq!(stages.len(), 1, "one stage reads an index: {explained}");
        (stages[0]["stage"].clone(), stages[0]["visited"].clone())
    };
    assert_eq!(reading(vec![one.clone(), role.clone()]), (json!("where"), json!(16)));
    assert_eq!(reading(vec![role, one]), (json!("withRole"), json!(16)));
}
