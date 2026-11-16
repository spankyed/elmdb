//! The data and every derived read, as the engine answers them and as the model says they must be

use super::*;

/// The data as the model compares it: each entity's fields, whether it is trashed and its roles, and every edge
pub(super) type State = (BTreeMap<String, (BTreeMap<String, Json>, bool, Vec<String>)>, Vec<(String, String, String)>);

/// The engine's data, as the model holds it
pub(super) fn state_of(db: &Database) -> State {
    let mut entities = BTreeMap::new();
    for id in IDS {
        let Some(Json::Object(row)) = row(db, id) else { continue };
        let trashed = row.contains_key("trashedAt");
        let roles = row
            .get("role")
            .and_then(Json::as_array)
            .map(|r| r.iter().map(|v| v.as_str().unwrap().to_owned()).collect())
            .unwrap_or_default();
        let fields = row.into_iter().filter(|(k, _)| !ENGINE_FIELDS.contains(&k.as_str())).collect();
        entities.insert(id.to_owned(), (fields, trashed, roles));
    }
    let edges = stored_edges(db)
        .into_iter()
        .map(|e| {
            let mut parts = e.split('|').map(str::to_owned);
            (parts.next().unwrap(), parts.next().unwrap(), parts.next().unwrap())
        })
        .collect::<Vec<_>>();
    let mut edges = edges;
    edges.sort();
    (entities, edges)
}

pub(super) fn model_state(m: &Model) -> State {
    let entities =
        m.entities.iter().map(|(id, e)| (id.clone(), (e.fields.clone(), e.trashed, e.roles.clone()))).collect();
    (entities, all_edges(m))
}

/// Every edge the model holds by its ends, a `cites` edge once for each key its pair holds, sorted
pub(super) fn all_edges(m: &Model) -> Vec<(String, String, String)> {
    let mut edges: Vec<_> = m.edges.iter().cloned().collect();
    edges.extend(m.keyed.iter().map(|(s, t, _)| (s.clone(), KEYED.to_owned(), t.clone())));
    edges.sort();
    edges
}

/// The ids of the entities a value index files under a key
pub(super) fn filed(s: &impl Read, entity_type: &str, own: &[u8]) -> Vec<String> {
    super::super::filed_under(s, entity_type, own).into_iter().map(|(_, id)| id).collect()
}

pub(super) fn ids_under(s: &impl Read, prefix: &[u8]) -> Vec<String> {
    s.scan(prefix).unwrap().into_iter().map(|(_, v)| values::decode(&v).unwrap().as_str().unwrap().to_owned()).collect()
}

/// Every derived read, as the engine's keys answer it
pub(super) fn derived_of(db: &Database) -> BTreeMap<String, Json> {
    db.snapshot(|s| {
        let mut out = BTreeMap::new();
        for t in ["Task", "Note"] {
            out.insert(format!("{t} live"), json!(ids_under(s, &keys::type_range(false, t))));
            out.insert(format!("{t} trashed"), json!(ids_under(s, &keys::type_range(true, t))));
            out.insert(format!("{t} count"), json!(entities::count(s, t).unwrap()));
            for role in ROLES {
                out.insert(
                    format!("{t} role {role}"),
                    json!(filed(s, t, &keys::index_range(t, "role", &[Part::from(role)]))),
                );
            }
        }
        // Task's index on `a`: every entry, as (part, id)
        let mut entries = Vec::new();
        for (k, id) in super::super::filed_under(s, "Task", &keys::index_range("Task", "a", &[])) {
            let parts = decode_key(&k).unwrap();
            entries.push(json!([format!("{:?}", parts[3]), id]));
        }
        out.insert("Task index a".to_owned(), json!(entries));
        for kind in ALL_KINDS {
            out.insert(format!("{kind} relations"), json!(relations::count(s, kind).unwrap()));
        }
        for id in IDS {
            for kind in ALL_KINDS {
                let far = |outward| {
                    relations::edges(s, id, outward, Some(kind))
                        .unwrap()
                        .into_iter()
                        .map(|(_, f)| f)
                        .collect::<Vec<_>>()
                };
                out.insert(format!("{id} out {kind}"), json!(far(true)));
                out.insert(format!("{id} in {kind}"), json!(far(false)));
            }
        }
        Ok(out)
    })
    .unwrap()
}

/// What the derived reads should answer, from the model
pub(super) fn derived_from(m: &Model) -> BTreeMap<String, Json> {
    let mut out = BTreeMap::new();
    for kind in ALL_KINDS {
        out.insert(format!("{kind} relations"), json!(all_edges(m).iter().filter(|e| e.1 == kind).count()));
    }
    let mut by_seq: Vec<(&String, &Entity)> = m.entities.iter().collect();
    by_seq.sort_by_key(|(_, e)| e.seq);
    for t in ["Task", "Note"] {
        let of_type = || by_seq.iter().filter(move |(id, _)| crate::engine::ids::type_of(id) == t);
        out.insert(
            format!("{t} live"),
            json!(of_type().filter(|(_, e)| !e.trashed).map(|(id, _)| id).collect::<Vec<_>>()),
        );
        out.insert(
            format!("{t} trashed"),
            json!(of_type().filter(|(_, e)| e.trashed).map(|(id, _)| id).collect::<Vec<_>>()),
        );
        out.insert(format!("{t} count"), json!(of_type().filter(|(_, e)| !e.trashed).count()));
        for role in ROLES {
            let holders: Vec<&String> =
                of_type().filter(|(_, e)| e.roles.iter().any(|r| r == role)).map(|(id, _)| *id).collect();
            out.insert(format!("{t} role {role}"), json!(holders));
        }
    }
    // The index's entries sort by part, then by place: rebuild that order from the model
    let mut entries: Vec<(Vec<u8>, u64, String, String)> = Vec::new();
    for (id, e) in by_seq.iter().filter(|(id, _)| crate::engine::ids::type_of(id) == "Task") {
        let value = e.fields.get("a").map(|v| serde_json::from_value::<rmpv::Value>(v.clone()).unwrap());
        for part in values::index_parts(value.as_ref()) {
            entries.push((
                crate::format::key::encode(std::slice::from_ref(&part)),
                e.seq,
                format!("{part:?}"),
                (*id).clone(),
            ));
        }
    }
    entries.sort();
    out.insert(
        "Task index a".to_owned(),
        json!(entries.into_iter().map(|(_, _, p, id)| json!([p, id])).collect::<Vec<_>>()),
    );
    // Far ends in the order linked: the model links in step order, which is each edge's place; recompute from the
    // engine's own order isn't the model's job, so compare as sets of far ends in a stable order
    let edges = all_edges(m);
    for id in IDS {
        for kind in ALL_KINDS {
            let all = || edges.iter();
            let mut outs: Vec<&String> = all().filter(|(s, k, _)| s == id && k == kind).map(|(_, _, t)| t).collect();
            let mut ins: Vec<&String> = all().filter(|(_, k, t)| t == id && k == kind).map(|(s, _, _)| s).collect();
            outs.sort();
            ins.sort();
            out.insert(format!("{id} out {kind}"), json!(outs));
            out.insert(format!("{id} in {kind}"), json!(ins));
        }
    }
    out
}

/// The engine's far ends sorted, since the model doesn't keep link order
pub(super) fn sorted_far_ends(mut derived: BTreeMap<String, Json>) -> BTreeMap<String, Json> {
    for (key, value) in derived.iter_mut() {
        if (key.contains(" out ") || key.contains(" in "))
            && let Json::Array(items) = value
        {
            items.sort_by(|a, b| a.as_str().cmp(&b.as_str()));
        }
    }
    derived
}

/// The words a search of Note's text index (its fields `a` and `b`) asks for: strings the workload writes
pub(super) const SEARCHED: [&str; 4] = ["a", "ab", "abc", "c"];

/// The substring and pattern searches of Note's trigram index (its fields `a` and `b`): `containing` with a text and
/// whether it ignores case, or `matching` with a pattern. Short texts narrow by nothing, which walks the type
pub(super) const GREPS: [(&str, &str, bool); 9] = [
    ("containing", "abc", false),
    ("containing", "ab", false),
    ("containing", "BCA", true),
    ("containing", "BCA", false),
    ("containing", "5555", false),
    ("matching", "a.c", false),
    ("matching", "^(ab|ca)c", false),
    ("matching", "c{3}|bab", false),
    ("matching", "[0-9]{4}", false),
];

pub(super) fn grep_step(n: usize) -> Json {
    match GREPS[n] {
        ("containing", text, case_insensitive) => {
            json!({ "step": "containing", "text": text, "caseInsensitive": case_insensitive })
        }
        (_, pattern, _) => json!({ "step": "matching", "pattern": pattern }),
    }
}

/// Whether a scan of the fields finds what grep `n` asks: a string, or a list's strings
pub(super) fn grepped(fields: &BTreeMap<String, Json>, n: usize) -> bool {
    let (kind, text, case_insensitive) = GREPS[n];
    let pattern = regex::Regex::new(text).unwrap();
    let found = |s: &str| match (kind, case_insensitive) {
        ("containing", false) => s.contains(text),
        ("containing", true) => s.to_lowercase().contains(&text.to_lowercase()),
        _ => pattern.is_match(s),
    };
    ["a", "b"].iter().any(|f| match fields.get(*f) {
        Some(Json::String(s)) => found(s),
        Some(Json::Array(items)) => items.iter().filter_map(Json::as_str).any(found),
        _ => false,
    })
}

/// What a `where` asks for: every number the workload writes, alone or in a list, and two of its long strings
pub(super) fn probes() -> Vec<Json> {
    let mut out: Vec<Json> = (0..10).map(Json::from).collect();
    out.push(Json::from("0".repeat(2100)));
    out.push(Json::from("5".repeat(2100)));
    out
}

/// The derived reads as data: a label and its read, a query plan answering ids or a count
pub(super) fn derived_reads() -> Vec<(String, Json)> {
    let mut out = Vec::new();
    let ids = |start: Json, steps: Json, scope: &str| json!({ "read": "query", "plan": { "scope": scope, "start": start, "steps": steps, "terminal": { "answer": "ids" } } });
    for t in ["Task", "Note"] {
        let start = json!({ "from": "types", "types": [t] });
        out.push((format!("{t} live"), ids(start.clone(), json!([]), "live")));
        out.push((format!("{t} trashed"), ids(start.clone(), json!([]), "trashed")));
        out.push((format!("{t} with trashed"), ids(start.clone(), json!([]), "withTrashed")));
        out.push((
            format!("{t} count"),
            json!({ "read": "query", "plan": { "start": start, "terminal": { "answer": "count" } } }),
        ));
        for probe in probes() {
            for field in ["a", "b"] {
                let label = format!(
                    "{t} where {field} {}",
                    if probe.is_string() {
                        format!("long {}", &probe.as_str().unwrap()[..1])
                    } else {
                        probe.to_string()
                    }
                );
                out.push((
                    label,
                    ids(start.clone(), json!([{ "step": "where", "field": field, "value": probe }]), "live"),
                ));
            }
        }
        // Ordered, whole and its first one or three, the path that keeps only the first: and a filter's first two
        for field in ["a", "b"] {
            for descending in [false, true] {
                for limit in [None, Some(1), Some(3)] {
                    let mut steps = vec![json!({ "step": "orderBy", "field": field, "descending": descending })];
                    steps.extend(limit.map(|n| json!({ "step": "limit", "n": n })));
                    let label = format!(
                        "{t} order {field} {} {}",
                        if descending { "desc" } else { "asc" },
                        limit.map_or("all".to_owned(), |n| n.to_string())
                    );
                    out.push((label, ids(start.clone(), json!(steps), "live")));
                }
            }
            for probe in [3, 5] {
                let steps = json!([{ "step": "where", "field": field, "value": probe }, { "step": "limit", "n": 2 }]);
                out.push((format!("{t} first 2 where {field} {probe}"), ids(start.clone(), steps, "live")));
            }
            // A range of numbers, with both bounds and with one; and one put in order, its first two
            let between = json!({ "step": "range", "field": field, "atLeast": 2, "below": 6 });
            out.push((format!("{t} range {field} 2 6"), ids(start.clone(), json!([between]), "live")));
            let above = json!({ "step": "range", "field": field, "above": 4 });
            out.push((format!("{t} range {field} 4 up"), ids(start.clone(), json!([above]), "live")));
            for descending in [false, true] {
                let order = json!({ "step": "orderBy", "field": field, "descending": descending });
                let direction = if descending { "desc" } else { "asc" };
                let steps = json!([between, order, { "step": "limit", "n": 2 }]);
                out.push((format!("{t} ranged {field} {direction}"), ids(start.clone(), steps, "live")));
                // The first two in the field's order of those the other field's value keeps: a task's `a` under
                // its `b` is read from the index on the two, and the rest are sorted
                let other = if field == "a" { "b" } else { "a" };
                let kept = json!({ "step": "where", "field": other, "value": 3 });
                let steps = json!([kept, order, { "step": "limit", "n": 2 }]);
                out.push((format!("{t} filtered {field} {direction}"), ids(start.clone(), steps, "live")));
                // And of those, the ones in the range, whichever of the two is asked first, and a page after each id
                for (way, steps) in [("after", json!([kept, between])), ("before", json!([between, kept]))] {
                    out.push((format!("{t} filtered range {field} {way}"), ids(start.clone(), steps, "live")));
                }
                for cursor in IDS {
                    let steps = json!([kept, order, { "step": "after", "id": cursor }, { "step": "limit", "n": 2 }]);
                    let label = format!("{t} filtered page {field} {direction} {cursor}");
                    out.push((label, ids(start.clone(), steps, "live")));
                }
                // A page of two after each of the type's ids, and after one of the other type's, in the field's order
                for cursor in IDS {
                    let steps = json!([order, { "step": "after", "id": cursor }, { "step": "limit", "n": 2 }]);
                    out.push((format!("{t} page {field} {direction} {cursor}"), ids(start.clone(), steps, "live")));
                }
            }
        }
        // A page of two after each id in the order the type was created in, and among the holders of a value
        for cursor in IDS {
            let steps = json!([{ "step": "after", "id": cursor }, { "step": "limit", "n": 2 }]);
            out.push((format!("{t} after {cursor}"), ids(start.clone(), steps, "live")));
            let steps = json!([
                { "step": "where", "field": "a", "value": 3 },
                { "step": "after", "id": cursor },
                { "step": "limit", "n": 2 }
            ]);
            out.push((format!("{t} holders after {cursor}"), ids(start.clone(), steps, "live")));
        }
        if t == "Note" {
            for n in 0..GREPS.len() {
                out.push((format!("{t} grep {n}"), ids(start.clone(), json!([grep_step(n)]), "live")));
                out.push((
                    format!("{t} grep {n} with trashed"),
                    ids(start.clone(), json!([grep_step(n)]), "withTrashed"),
                ));
                // Among a set of both types: Task has no trigram index, so none of its entities matches
                out.push((
                    format!("{t} grep {n} among"),
                    ids(json!({ "from": "ids", "ids": IDS }), json!([grep_step(n)]), "live"),
                ));
            }
            for word in SEARCHED {
                let steps = json!([{ "step": "search", "query": word, "options": { "fuzzy": false } }]);
                out.push((format!("{t} search {word}"), ids(start.clone(), steps, "live")));
            }
        }
        for role in ROLES {
            out.push((
                format!("{t} role {role}"),
                ids(start.clone(), json!([{ "step": "withRole", "role": role }]), "live"),
            ));
            // A role's holders that hold a value in `a`, or one in a range of it, whichever of the two filters is
            // asked first: a task has an index for each, and the one holding the fewer entries is read
            let held = json!({ "step": "withRole", "role": role });
            let filters = [
                ("3", json!({ "step": "where", "field": "a", "value": 3 })),
                ("5", json!({ "step": "where", "field": "a", "value": 5 })),
                ("range", json!({ "step": "range", "field": "a", "atLeast": 2, "below": 6 })),
            ];
            for (asked, filter) in filters {
                for (way, steps) in [("after", json!([filter, held])), ("before", json!([held, filter]))] {
                    out.push((format!("{t} paired {way} {role} {asked}"), ids(start.clone(), steps, "live")));
                }
            }
        }
    }
    for id in IDS {
        for kind in ALL_KINDS {
            for step in ["out", "in"] {
                out.push((
                    format!("{id} {step} {kind}"),
                    ids(json!({ "from": "ids", "ids": [id] }), json!([{ "step": step, "kinds": [kind] }]), "live"),
                ));
            }
        }
    }
    out
}

/// The engine's answers to every derived read, in one call; far ends sorted, since the model doesn't keep link order
pub(super) fn plans_of(db: &Database) -> BTreeMap<String, Json> {
    let reads = derived_reads();
    let request = crate::engine::plan::Reads {
        reads: reads.iter().map(|(_, r)| serde_json::from_value(r.clone()).unwrap()).collect(),
        track: None,
    };
    let answers: Json =
        serde_json::to_value(rmpv::decode::read_value(&mut &db.read(&request).unwrap()[..]).unwrap()).unwrap();
    reads
        .into_iter()
        .zip(answers.as_array().unwrap())
        .map(|((label, _), answer)| {
            let mut value = if answer["kind"] == "count" { answer["n"].clone() } else { answer["ids"].clone() };
            if (label.contains(" out ") || label.contains(" in ") || label.contains(" search "))
                && let Json::Array(items) = &mut value
            {
                items.sort_by(|a, b| a.as_str().cmp(&b.as_str()));
            }
            (label, value)
        })
        .collect()
}

/// How `orderBy` orders two values: booleans, numbers, strings by the root collation and then their bytes, a list or
/// map by its JSON, then a missing or null value
pub(super) fn ordered_as(
    collator: &icu_collator::CollatorBorrowed<'static>,
    a: Option<&Json>,
    b: Option<&Json>,
) -> std::cmp::Ordering {
    let class = |v: Option<&Json>| match v {
        None | Some(Json::Null) => 4,
        Some(Json::Bool(_)) => 0,
        Some(Json::Number(_)) => 1,
        Some(Json::String(_)) => 2,
        Some(_) => 3,
    };
    class(a).cmp(&class(b)).then_with(|| match (a, b) {
        (Some(Json::Bool(x)), Some(Json::Bool(y))) => x.cmp(y),
        (Some(Json::Number(x)), Some(Json::Number(y))) => x.as_f64().partial_cmp(&y.as_f64()).unwrap(),
        (Some(Json::String(x)), Some(Json::String(y))) => collator.compare(x, y).then_with(|| x.cmp(y)),
        (Some(x), Some(y)) if class(a) == 3 => collator.compare(&x.to_string(), &y.to_string()),
        _ => std::cmp::Ordering::Equal,
    })
}

/// Whether a `range` holds a value: a number `within` it, or a list holding one
pub(super) fn in_range(value: Option<&Json>, within: &impl Fn(f64) -> bool) -> bool {
    match value {
        Some(Json::Array(items)) => items.iter().filter_map(Json::as_f64).any(within),
        Some(value) => value.as_f64().is_some_and(within),
        None => false,
    }
}

/// The two ids after `cursor` among `ids`, or none when it isn't one of them
pub(super) fn page_after<'a>(ids: impl Iterator<Item = &'a String>, cursor: &str) -> Vec<&'a String> {
    ids.skip_while(|id| *id != cursor).skip(1).take(2).collect()
}

/// What the derived reads should answer, from the model
pub(super) fn plans_from(m: &Model) -> BTreeMap<String, Json> {
    let mut by_seq: Vec<(&String, &Entity)> = m.entities.iter().collect();
    by_seq.sort_by_key(|(_, e)| e.seq);
    let live = |id: &str| m.entities.get(id).is_some_and(|e| !e.trashed);
    let holds = |stored: Option<&Json>, probe: &Json| match stored {
        None => false,
        Some(Json::Array(items)) => items.iter().any(|v| v == probe),
        Some(v) => v == probe,
    };
    let collator = icu_collator::CollatorBorrowed::try_new(Default::default(), Default::default()).unwrap();
    let mut out = BTreeMap::new();
    for (label, _) in derived_reads() {
        let words: Vec<&str> = label.split(' ').collect();
        let value = if IDS.contains(&words[0]) {
            let (id, step, kind) = (words[0], words[1], words[2]);
            let edges = all_edges(m);
            let mut far: Vec<&String> = if !live(id) {
                vec![]
            } else {
                let all = edges.iter();
                let mut ends: Vec<&String> = if step == "out" {
                    all.filter(|(s, k, t)| s == id && k == kind && t != id).map(|(_, _, t)| t).collect()
                } else {
                    all.filter(|(s, k, t)| t == id && k == kind && s != id).map(|(s, _, _)| s).collect()
                };
                ends.retain(|e| live(e));
                ends
            };
            far.sort();
            far.dedup();
            json!(far)
        } else {
            let t = words[0];
            let of_type = || by_seq.iter().filter(move |(id, _)| crate::engine::ids::type_of(id) == t);
            match &words[1..] {
                ["live"] => json!(of_type().filter(|(_, e)| !e.trashed).map(|(id, _)| id).collect::<Vec<_>>()),
                ["trashed"] => json!(of_type().filter(|(_, e)| e.trashed).map(|(id, _)| id).collect::<Vec<_>>()),
                ["with", "trashed"] => json!(of_type().map(|(id, _)| id).collect::<Vec<_>>()),
                ["count"] => json!(of_type().filter(|(_, e)| !e.trashed).count()),
                // A field's text is a string or a list's strings; the workload's strings are one word each, held as
                // the word they fold to. Ranking is the corpora specs' (`text/tests.rs`): a match set is ordered here
                // as the engine's ties are
                ["search", word] => {
                    let mut found: Vec<&String> = of_type()
                        .filter(|(_, e)| {
                            !e.trashed
                                && ["a", "b"].iter().any(|f| match e.fields.get(*f) {
                                    Some(Json::String(s)) => folded(s) == *word,
                                    Some(Json::Array(items)) => {
                                        items.iter().filter_map(Json::as_str).any(|s| folded(s) == *word)
                                    }
                                    _ => false,
                                })
                        })
                        .map(|(id, _)| *id)
                        .collect();
                    found.sort();
                    json!(found)
                }
                ["grep", n, rest @ ..] => {
                    let n: usize = n.parse().unwrap();
                    match rest {
                        [] => json!(
                            of_type()
                                .filter(|(_, e)| !e.trashed && grepped(&e.fields, n))
                                .map(|(id, _)| id)
                                .collect::<Vec<_>>()
                        ),
                        ["with", "trashed"] => {
                            json!(
                                of_type().filter(|(_, e)| grepped(&e.fields, n)).map(|(id, _)| id).collect::<Vec<_>>()
                            )
                        }
                        ["among"] => json!(
                            IDS.iter()
                                .filter(|id| crate::engine::ids::type_of(id) == t)
                                .filter(|id| m.entities.get(**id).is_some_and(|e| !e.trashed && grepped(&e.fields, n)))
                                .collect::<Vec<_>>()
                        ),
                        other => unreachable!("{other:?}"),
                    }
                }
                ["role", role] => json!(
                    of_type()
                        .filter(|(_, e)| !e.trashed && e.roles.iter().any(|r| r == role))
                        .map(|(id, _)| id)
                        .collect::<Vec<_>>()
                ),
                ["paired", _, role, asked] => {
                    let within = |n: f64| (2.0..6.0).contains(&n);
                    let kept = |e: &Entity| match asked.parse::<i64>() {
                        Ok(probe) => holds(e.fields.get("a"), &Json::from(probe)),
                        Err(_) => in_range(e.fields.get("a"), &within),
                    };
                    json!(
                        of_type()
                            .filter(|(_, e)| !e.trashed && e.roles.iter().any(|r| r == role) && kept(e))
                            .map(|(id, _)| id)
                            .collect::<Vec<_>>()
                    )
                }
                ["order", field, direction, limit] => {
                    let mut ordered: Vec<(&String, &Entity)> = of_type().filter(|(_, e)| !e.trashed).copied().collect();
                    ordered.sort_by(|(_, x), (_, y)| {
                        let o = ordered_as(&collator, x.fields.get(*field), y.fields.get(*field));
                        if *direction == "desc" { o.reverse() } else { o }
                    });
                    let take = limit.parse().unwrap_or(usize::MAX);
                    json!(ordered.into_iter().take(take).map(|(id, _)| id).collect::<Vec<_>>())
                }
                ["range", field, low, high] => {
                    let low: f64 = low.parse().unwrap();
                    let within = |n: f64| match high.parse::<f64>() {
                        Ok(high) => low <= n && n < high,
                        Err(_) => low < n,
                    };
                    json!(
                        of_type()
                            .filter(|(_, e)| !e.trashed && in_range(e.fields.get(*field), &within))
                            .map(|(id, _)| id)
                            .collect::<Vec<_>>()
                    )
                }
                ["ranged", field, direction] => {
                    let within = |n: f64| (2.0..6.0).contains(&n);
                    let mut ordered: Vec<(&String, &Entity)> = of_type()
                        .filter(|(_, e)| !e.trashed && in_range(e.fields.get(*field), &within))
                        .copied()
                        .collect();
                    ordered.sort_by(|(_, x), (_, y)| {
                        let o = ordered_as(&collator, x.fields.get(*field), y.fields.get(*field));
                        if *direction == "desc" { o.reverse() } else { o }
                    });
                    json!(ordered.into_iter().take(2).map(|(id, _)| id).collect::<Vec<_>>())
                }
                ["filtered", field, direction] => {
                    let other = if *field == "a" { "b" } else { "a" };
                    let mut ordered: Vec<(&String, &Entity)> = of_type()
                        .filter(|(_, e)| !e.trashed && holds(e.fields.get(other), &json!(3)))
                        .copied()
                        .collect();
                    ordered.sort_by(|(_, x), (_, y)| {
                        let o = ordered_as(&collator, x.fields.get(*field), y.fields.get(*field));
                        if *direction == "desc" { o.reverse() } else { o }
                    });
                    json!(ordered.into_iter().take(2).map(|(id, _)| id).collect::<Vec<_>>())
                }
                ["filtered", "range", field, _] => {
                    let other = if *field == "a" { "b" } else { "a" };
                    let within = |n: f64| (2.0..6.0).contains(&n);
                    let kept =
                        |e: &Entity| holds(e.fields.get(other), &json!(3)) && in_range(e.fields.get(*field), &within);
                    json!(of_type().filter(|(_, e)| !e.trashed && kept(e)).map(|(id, _)| id).collect::<Vec<_>>())
                }
                ["filtered", "page", field, direction, cursor] => {
                    let other = if *field == "a" { "b" } else { "a" };
                    let mut ordered: Vec<(&String, &Entity)> = of_type()
                        .filter(|(_, e)| !e.trashed && holds(e.fields.get(other), &json!(3)))
                        .copied()
                        .collect();
                    ordered.sort_by(|(_, x), (_, y)| {
                        let o = ordered_as(&collator, x.fields.get(*field), y.fields.get(*field));
                        if *direction == "desc" { o.reverse() } else { o }
                    });
                    json!(page_after(ordered.into_iter().map(|(id, _)| id), cursor))
                }
                ["page", field, direction, cursor] => {
                    let mut ordered: Vec<(&String, &Entity)> = of_type().filter(|(_, e)| !e.trashed).copied().collect();
                    ordered.sort_by(|(_, x), (_, y)| {
                        let o = ordered_as(&collator, x.fields.get(*field), y.fields.get(*field));
                        if *direction == "desc" { o.reverse() } else { o }
                    });
                    json!(page_after(ordered.into_iter().map(|(id, _)| id), cursor))
                }
                ["after", cursor] => {
                    json!(page_after(of_type().filter(|(_, e)| !e.trashed).map(|(id, _)| *id), cursor))
                }
                ["holders", "after", cursor] => json!(page_after(
                    of_type().filter(|(_, e)| !e.trashed && holds(e.fields.get("a"), &json!(3))).map(|(id, _)| *id),
                    cursor
                )),
                ["first", n, "where", field, probe] => {
                    let probe = Json::from(probe.parse::<i64>().unwrap());
                    json!(
                        of_type()
                            .filter(|(_, e)| !e.trashed && holds(e.fields.get(*field), &probe))
                            .take(n.parse().unwrap())
                            .map(|(id, _)| id)
                            .collect::<Vec<_>>()
                    )
                }
                ["where", field, rest @ ..] => {
                    let probe = match rest {
                        ["long", digit] => Json::from(digit.repeat(2100)),
                        [n] => Json::from(n.parse::<i64>().unwrap()),
                        _ => unreachable!(),
                    };
                    json!(
                        of_type()
                            .filter(|(_, e)| !e.trashed && holds(e.fields.get(*field), &probe))
                            .map(|(id, _)| id)
                            .collect::<Vec<_>>()
                    )
                }
                other => unreachable!("{other:?}"),
            }
        };
        out.insert(label, value);
    }
    out
}

/// A word the workload writes as the text index holds it: small, its accents dropped, composed or apart
pub(super) fn folded(word: &str) -> String {
    word.to_lowercase()
        .chars()
        .filter(|c| !ACCENTS.iter().any(|(.., mark)| c == mark))
        .map(|c| ACCENTS.iter().find(|(small, ..)| c == *small).map_or(c, |(_, _, letter, _)| *letter))
        .collect()
}
