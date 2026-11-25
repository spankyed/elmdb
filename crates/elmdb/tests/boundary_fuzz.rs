//! Deterministic Rust boundary fuzzing and query governance tests: msgpack validation on every byte boundary
//! (`read_bytes`, `write_bytes`, `bulk_load_bytes`, `changed_since_bytes`, `dump::load`), recursive list schema depth
//! cap, graph algorithm iteration bounds, streaming `reach` early-stop under `limit`, and payload-free `explain()`
//! telemetry.

use std::collections::BTreeMap;

use elmdb::dump;
use elmdb::engine::answer;
use elmdb::engine::batch::Batch;
use elmdb::engine::changed::Position;
use elmdb::engine::maintenance::Load;
use elmdb::engine::plan::{Direction, Plan, ReadOp, Reads, Scope, Start, Step, Terminal};
use elmdb::schema::{MAX_LIST_DEPTH, SCHEMA_LANGUAGE, Schema};
use elmdb::{Database, ErrorCode, OpenOptions};
use serde_json::{Value as Json, json};

const MAX_ITERATIONS: u32 = 10_000;
const MAX_STEPS: usize = 256;

struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed.max(1))
    }

    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        self.0
    }

    fn usize(&mut self, bound: usize) -> usize {
        (self.next_u64() as usize) % bound
    }

    fn bytes(&mut self, max_len: usize) -> Vec<u8> {
        let len = self.usize(max_len + 1);
        (0..len).map(|_| (self.next_u64() >> 56) as u8).collect()
    }

    fn value(&mut self, depth: usize) -> rmpv::Value {
        match self.usize(8) {
            0 => rmpv::Value::Nil,
            1 => rmpv::Value::Boolean(self.next_u64() & 1 == 0),
            2 => rmpv::Value::from((self.next_u64() as i64) % 10_000),
            3 => rmpv::Value::from(((self.next_u64() % 1000) as f64) * 0.25),
            4 => rmpv::Value::from("x".repeat(self.usize(24))),
            5 => rmpv::Value::Binary(self.bytes(16)),
            6 if depth < 3 => {
                let n = self.usize(4);
                rmpv::Value::Array((0..n).map(|_| self.value(depth + 1)).collect())
            }
            7 if depth < 3 => {
                const KEYS: [&str; 8] = ["op", "read", "type", "id", "ops", "reads", "plan", "sequence"];
                let n = self.usize(4);
                rmpv::Value::Map(
                    (0..n).map(|_| (rmpv::Value::from(KEYS[self.usize(KEYS.len())]), self.value(depth + 1))).collect(),
                )
            }
            _ => rmpv::Value::Nil,
        }
    }
}

fn encode_value(value: &rmpv::Value) -> Vec<u8> {
    let mut out = Vec::new();
    rmpv::encode::write_value(&mut out, value).unwrap();
    out
}

fn test_schema() -> Schema {
    Schema::from_json(&json!({
        "$schema": SCHEMA_LANGUAGE,
        "relations": {
            "refs": { "cardinality": "many-to-many" },
            "child": { "cardinality": "many-to-many" }
        },
        "types": {
            "Note": {
                "changeOrder": true,
                "fields": {
                    "title": { "type": "string", "optional": true },
                    "body": { "type": "string", "optional": true },
                    "n": { "type": "number", "optional": true }
                },
                "indexes": {
                    "grams": { "kind": "trigram", "fields": ["title", "body"] },
                    "text": { "kind": "text", "fields": { "title": {}, "body": {} } },
                    "meaning": { "kind": "vector", "from": ["body"], "dims": 3, "model": "m1" }
                }
            },
            "Bridge": {
                "fields": {
                    "label": { "type": "string", "optional": true }
                }
            }
        }
    }))
    .unwrap()
}

fn open_db(dir: &tempfile::TempDir) -> Database {
    Database::open(dir.path(), OpenOptions { schema: Some(test_schema()), syncs: false, ..Default::default() }).unwrap()
}

fn write_ops(db: &Database, ops: Vec<Json>) {
    let batch: Batch = serde_json::from_value(json!({ "ops": ops })).unwrap();
    db.write(&batch, false).unwrap();
}

fn read_explained(db: &Database, plan: Json) -> Json {
    let reads: Reads =
        serde_json::from_value(json!({ "reads": [{ "read": "query", "explain": true, "plan": plan }] })).unwrap();
    let bytes = db.read(&reads).unwrap();
    answer::conforms(&bytes).unwrap_or_else(|e| panic!("explained answer failed conformance: {e}"));
    let mut decoded = serde_json::to_value(rmpv::decode::read_value(&mut &bytes[..]).unwrap()).unwrap();
    decoded[0].take()
}

fn list_schema_with_depth(depth: usize) -> Json {
    let mut field = json!({ "type": "string" });
    for _ in 0..depth {
        field = json!({ "type": "list", "of": field });
    }
    json!({
        "$schema": SCHEMA_LANGUAGE,
        "types": {
            "Item": {
                "fields": {
                    "nested": field
                }
            }
        }
    })
}

#[test]
fn schema_list_depth_accepts_max_and_refuses_one_deeper() {
    assert!(Schema::from_json(&list_schema_with_depth(MAX_LIST_DEPTH)).is_ok());

    let err = Schema::from_json(&list_schema_with_depth(MAX_LIST_DEPTH + 1)).unwrap_err();
    assert_eq!(err.code, ErrorCode::SchemaInvalid);
    assert!(err.message.contains(&format!("at most {MAX_LIST_DEPTH}")), "unexpected message: {}", err.message);

    // Much deeper nesting also refuses cleanly without stack overflow
    let err = Schema::from_json(&list_schema_with_depth(200)).unwrap_err();
    assert_eq!(err.code, ErrorCode::SchemaInvalid);
}

#[test]
fn changed_since_bytes_validates_position_before_decode() {
    let dir = tempfile::tempdir().unwrap();
    let db = open_db(&dir);
    write_ops(&db, vec![json!({ "op": "create", "type": "Note", "id": "Note-1", "fields": { "title": "hello" } })]);
    let types = vec!["Note".to_owned()];

    let valid = rmp_serde::to_vec_named(&Position::default()).unwrap();
    assert!(db.changed_since_bytes(&types, &valid, 10).is_ok());

    // Trailing garbage byte after a valid Position must be rejected with BatchInvalid
    let mut trailing = valid.clone();
    trailing.push(0xc0);
    let err = db.changed_since_bytes(&types, &trailing, 10).unwrap_err();
    assert_eq!(err.code, ErrorCode::BatchInvalid, "{err}");

    // Over-deep nesting (> 256) must be rejected with BatchInvalid
    let mut over_deep = vec![0x91; 260];
    over_deep.push(0xc0);
    let err = db.changed_since_bytes(&types, &over_deep, 10).unwrap_err();
    assert_eq!(err.code, ErrorCode::BatchInvalid, "{err}");

    // Invalid UTF-8 string key/value inside msgpack map
    let bad_utf8 = [0x81, 0xa4, b'a', 0xed, 0xa0, 0x80, 0x00];
    let err = db.changed_since_bytes(&types, &bad_utf8, 10).unwrap_err();
    assert_eq!(err.code, ErrorCode::BatchInvalid, "{err}");

    // Disallowed msgpack extension type
    let bad_ext = encode_value(&rmpv::Value::Ext(42, vec![1, 2]));
    let err = db.changed_since_bytes(&types, &bad_ext, 10).unwrap_err();
    assert_eq!(err.code, ErrorCode::BatchInvalid, "{err}");

    // Reserved msgpack marker 0xc1
    let err = db.changed_since_bytes(&types, &[0xc1], 10).unwrap_err();
    assert_eq!(err.code, ErrorCode::BatchInvalid, "{err}");

    assert!(db.check().unwrap().is_clean());
}

#[test]
fn byte_boundaries_survive_seeded_fuzz_and_byte_flips() {
    let dir = tempfile::tempdir().unwrap();
    let db = open_db(&dir);
    let bulk_dir = tempfile::tempdir().unwrap();
    let bulk_db = open_db(&bulk_dir);
    let types = vec!["Note".to_owned()];

    let valid_batch = rmp_serde::to_vec_named(&json!({
        "ops": [
            { "op": "create", "type": "Note", "id": "Note-a", "fields": { "title": "alpha" } },
            { "op": "create", "type": "Note", "id": "Note-b", "fields": { "title": "beta" } }
        ]
    }))
    .unwrap();
    let valid_reads = rmp_serde::to_vec_named(&json!({
        "reads": [{ "read": "query", "plan": {
            "start": { "from": "types", "types": ["Note"] },
            "terminal": { "answer": "ids" }
        } }]
    }))
    .unwrap();
    let valid_loads = rmp_serde::to_vec_named(&vec![Load::Entity {
        id: "Note-load".into(),
        fields: BTreeMap::from([("title".into(), rmpv::Value::from("loaded"))]),
        trashed: false,
    }])
    .unwrap();
    let valid_pos = rmp_serde::to_vec_named(&Position::default()).unwrap();

    let assert_safe_err = |err: elmdb::Error, label: &str| {
        assert!(
            !matches!(err.code, ErrorCode::Corrupted | ErrorCode::Io | ErrorCode::Closed),
            "{label}: unexpected fatal error {err}"
        );
    };

    for seed in 1..=200u64 {
        let mut r = Rng::new(seed);
        let raw = r.bytes(64);
        let packed = encode_value(&r.value(0));
        for payload in [&raw, &packed] {
            if let Err(err) = db.read_bytes(payload) {
                assert_safe_err(err, &format!("seed {seed} read_bytes"));
            }
            if let Err(err) = db.write_bytes(payload, false) {
                assert_safe_err(err, &format!("seed {seed} write_bytes"));
            }
            if let Err(err) = bulk_db.bulk_load_bytes(payload) {
                assert_safe_err(err, &format!("seed {seed} bulk_load_bytes"));
            }
            if let Err(err) = db.changed_since_bytes(&types, payload, 16) {
                assert_eq!(err.code, ErrorCode::BatchInvalid, "seed {seed}: {err}");
            }
        }
        // Single-to-triple byte flips on valid payloads
        for (kind, source) in
            [("batch", &valid_batch), ("reads", &valid_reads), ("loads", &valid_loads), ("pos", &valid_pos)]
        {
            let mut mutated = source.clone();
            let flips = 1 + r.usize(3);
            for _ in 0..flips {
                let at = r.usize(mutated.len());
                mutated[at] ^= 1u8.wrapping_shl((r.next_u64() % 8) as u32);
            }
            match kind {
                "batch" => {
                    if let Err(err) = db.write_bytes(&mutated, false) {
                        assert_safe_err(err, &format!("seed {seed} batch flip"));
                    }
                }
                "reads" => {
                    if let Err(err) = db.read_bytes(&mutated) {
                        assert_safe_err(err, &format!("seed {seed} reads flip"));
                    }
                }
                "loads" => {
                    if let Err(err) = bulk_db.bulk_load_bytes(&mutated) {
                        assert_safe_err(err, &format!("seed {seed} loads flip"));
                    }
                }
                _ => {
                    if let Err(err) = db.changed_since_bytes(&types, &mutated, 16) {
                        assert_eq!(err.code, ErrorCode::BatchInvalid, "seed {seed} pos flip: {err}");
                    }
                }
            }
        }
    }

    // Pathological plan payloads: step chain > MAX_STEPS, invalid regex, NaN/Inf/oversized vector
    let many_steps: Vec<Json> = (0..=MAX_STEPS).map(|_| json!({ "step": "reverse" })).collect();
    let too_many_steps = rmp_serde::to_vec_named(&json!({
        "reads": [{ "read": "query", "plan": {
            "start": { "from": "types", "types": ["Note"] },
            "steps": many_steps,
            "terminal": { "answer": "ids" }
        } }]
    }))
    .unwrap();
    assert_eq!(db.read_bytes(&too_many_steps).unwrap_err().code, ErrorCode::BatchInvalid);

    let bad_regex = rmp_serde::to_vec_named(&json!({
        "reads": [{ "read": "query", "plan": {
            "start": { "from": "types", "types": ["Note"] },
            "steps": [{ "step": "matching", "pattern": "(unclosed[" }],
            "terminal": { "answer": "ids" }
        } }]
    }))
    .unwrap();
    assert_eq!(db.read_bytes(&bad_regex).unwrap_err().code, ErrorCode::PatternInvalid);

    // Oversized vector dims mismatch
    let wrong_dims = rmp_serde::to_vec_named(&json!({
        "reads": [{ "read": "query", "plan": {
            "start": { "from": "types", "types": ["Note"] },
            "steps": [{ "step": "near", "vector": vec![1.0f32; 128], "k": 5 }],
            "terminal": { "answer": "ids" }
        } }]
    }))
    .unwrap();
    assert_eq!(db.read_bytes(&wrong_dims).unwrap_err().code, ErrorCode::ValueInvalid);

    // Checked writes leave the database clean; unchecked bulk loads leave it readable and checkable
    assert!(db.check().unwrap().is_clean());
    assert!(bulk_db.check().is_ok());
    assert!(bulk_db.read_bytes(&valid_reads).is_ok());
}

#[test]
fn dump_load_rejects_truncated_corrupted_and_trailing_byte_frames() {
    let root = tempfile::tempdir().unwrap();
    let src_dir = root.path().join("src");
    let db = Database::open(&src_dir, OpenOptions { schema: Some(test_schema()), syncs: false, ..Default::default() })
        .unwrap();
    write_ops(&db, vec![json!({ "op": "create", "type": "Note", "id": "Note-1", "fields": { "title": "hi" } })]);
    let dump_path = root.path().join("valid.dump");
    dump::dump(&db, &dump_path).unwrap();
    db.close().unwrap();

    let valid_bytes = std::fs::read(&dump_path).unwrap();
    let target = root.path().join("loaded");

    // 1. Truncated at various points
    for cut in [0, 4, 8, 12, 16, valid_bytes.len() - 25, valid_bytes.len() - 1] {
        let bad = root.path().join(format!("trunc-{cut}.dump"));
        std::fs::write(&bad, &valid_bytes[..cut]).unwrap();
        let err = dump::load(&bad, &target).unwrap_err();
        assert_eq!(err.code, ErrorCode::WrongFormat, "cut={cut}: {err}");
        assert!(!target.join("data.mdb").exists());
    }

    // 2. Trailing garbage byte INSIDE a framed Load payload
    let mut with_trailing_frame = Vec::new();
    with_trailing_frame.extend_from_slice(b"ELMDBDMP");
    with_trailing_frame.extend_from_slice(&1u32.to_le_bytes());
    with_trailing_frame.extend_from_slice(&0u32.to_le_bytes()); // empty schema
    let mut frame =
        rmp_serde::to_vec_named(&Load::Entity { id: "Note-1".into(), fields: BTreeMap::new(), trashed: false })
            .unwrap();
    frame.push(0xc0); // trailing nil after valid Load
    with_trailing_frame.extend_from_slice(&(frame.len() as u32).to_le_bytes());
    with_trailing_frame.extend_from_slice(&frame);
    with_trailing_frame.extend_from_slice(&u32::MAX.to_le_bytes());
    with_trailing_frame.extend_from_slice(&1u64.to_le_bytes());
    with_trailing_frame.extend_from_slice(&1u64.to_le_bytes());

    let trailing_frame_path = root.path().join("trailing-frame.dump");
    std::fs::write(&trailing_frame_path, &with_trailing_frame).unwrap();
    let err = dump::load(&trailing_frame_path, &target).unwrap_err();
    assert_eq!(err.code, ErrorCode::WrongFormat, "trailing frame: {err}");
    assert!(!target.join("data.mdb").exists());

    // 3. Over-deep msgpack inside a load frame
    let mut deep_frame = vec![0x91; 260];
    deep_frame.push(0xc0);
    let mut with_deep = Vec::new();
    with_deep.extend_from_slice(b"ELMDBDMP");
    with_deep.extend_from_slice(&1u32.to_le_bytes());
    with_deep.extend_from_slice(&0u32.to_le_bytes());
    with_deep.extend_from_slice(&(deep_frame.len() as u32).to_le_bytes());
    with_deep.extend_from_slice(&deep_frame);
    with_deep.extend_from_slice(&u32::MAX.to_le_bytes());
    with_deep.extend_from_slice(&1u64.to_le_bytes());
    with_deep.extend_from_slice(&1u64.to_le_bytes());

    let deep_path = root.path().join("deep-frame.dump");
    std::fs::write(&deep_path, &with_deep).unwrap();
    let err = dump::load(&deep_path, &target).unwrap_err();
    assert_eq!(err.code, ErrorCode::WrongFormat, "deep frame: {err}");
    assert!(!target.join("data.mdb").exists());

    // Valid dump still loads cleanly
    let report = dump::load(&dump_path, &target).unwrap();
    assert_eq!(report.entities, 1);
}

#[test]
fn rank_and_communities_enforce_iteration_cap() {
    let dir = tempfile::tempdir().unwrap();
    let db = open_db(&dir);
    write_ops(
        &db,
        vec![
            json!({ "op": "create", "type": "Note", "id": "Note-1" }),
            json!({ "op": "create", "type": "Note", "id": "Note-2" }),
            json!({ "op": "link", "source": "Note-1", "kind": "refs", "target": "Note-2" }),
        ],
    );

    let run = |plan: Json| {
        let reads: Reads = serde_json::from_value(json!({ "reads": [{ "read": "query", "plan": plan }] })).unwrap();
        db.read(&reads)
    };

    // Rank: MAX_ITERATIONS succeeds; 0, MAX_ITERATIONS + 1, u32::MAX fail with BatchInvalid
    assert!(
        run(json!({
            "start": { "from": "types", "types": ["Note"] },
            "steps": [{ "step": "rank", "kinds": ["refs"], "iterations": MAX_ITERATIONS }],
            "terminal": { "answer": "ids" }
        }))
        .is_ok()
    );
    for bad in [0u32, MAX_ITERATIONS + 1, u32::MAX] {
        let err = run(json!({
            "start": { "from": "types", "types": ["Note"] },
            "steps": [{ "step": "rank", "kinds": ["refs"], "iterations": bad }],
            "terminal": { "answer": "ids" }
        }))
        .unwrap_err();
        assert_eq!(err.code, ErrorCode::BatchInvalid, "rank iterations={bad}: {err}");
    }

    // Communities: MAX_ITERATIONS succeeds; 0, MAX_ITERATIONS + 1, u32::MAX fail with BatchInvalid
    assert!(
        run(json!({
            "start": { "from": "types", "types": ["Note"] },
            "terminal": { "answer": "communities", "kinds": ["refs"], "iterations": MAX_ITERATIONS }
        }))
        .is_ok()
    );
    for bad in [0u32, MAX_ITERATIONS + 1, u32::MAX] {
        let err = run(json!({
            "start": { "from": "types", "types": ["Note"] },
            "terminal": { "answer": "communities", "kinds": ["refs"], "iterations": bad }
        }))
        .unwrap_err();
        assert_eq!(err.code, ErrorCode::BatchInvalid, "communities iterations={bad}: {err}");
    }

    // Non-finite rank tolerance passed directly over the Rust API must be refused with BatchInvalid
    for bad_tolerance in [f64::INFINITY, f64::NEG_INFINITY, f64::NAN] {
        let plan = Plan {
            scope: Scope::Live,
            start: Start::Types { types: vec!["Note".into()] },
            steps: vec![Step::Rank {
                kinds: vec!["refs".into()],
                direction: Direction::Out,
                seeds: None,
                weights: true,
                damping: 0.85,
                iterations: 100,
                tolerance: bad_tolerance,
            }],
            terminal: Terminal::Ids,
        };
        let err = db
            .read(&Reads { reads: vec![ReadOp::Query { plan, visits: false, explain: false }], track: None })
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::BatchInvalid, "rank tolerance={bad_tolerance}: {err}");
    }
}

#[test]
fn streaming_reach_stops_early_under_limit_and_preserves_order_and_filtering() {
    let dir = tempfile::tempdir().unwrap();
    let db = open_db(&dir);

    // Build a 3-level tree rooted at Note-0:
    // Note-0 -> Bridge-1..Bridge-4 -> Note-10..Note-49 (10 children per bridge = 40 leaf notes)
    let mut ops = vec![json!({ "op": "create", "type": "Note", "id": "Note-0" })];
    for b in 1..=4 {
        let bid = format!("Bridge-{b}");
        ops.push(json!({ "op": "create", "type": "Bridge", "id": bid }));
        ops.push(json!({ "op": "link", "source": "Note-0", "kind": "child", "target": bid }));
        for c in 0..10 {
            let nid = format!("Note-{b}-{c}");
            ops.push(json!({ "op": "create", "type": "Note", "id": nid }));
            ops.push(json!({ "op": "link", "source": bid, "kind": "child", "target": nid }));
        }
    }
    write_ops(&db, ops);

    // Full descendants (unfiltered) vs limit 2
    let full = read_explained(
        &db,
        json!({
            "start": { "from": "ids", "ids": ["Note-0"] },
            "steps": [{ "step": "descendants", "kinds": ["child"] }],
            "terminal": { "answer": "ids" }
        }),
    );
    let limited = read_explained(
        &db,
        json!({
            "start": { "from": "ids", "ids": ["Note-0"] },
            "steps": [
                { "step": "descendants", "kinds": ["child"] },
                { "step": "limit", "n": 2 }
            ],
            "terminal": { "answer": "ids" }
        }),
    );

    let full_ids: Vec<String> = serde_json::from_value(full["answer"]["ids"].clone()).unwrap();
    let limited_ids: Vec<String> = serde_json::from_value(limited["answer"]["ids"].clone()).unwrap();
    assert_eq!(full_ids.len(), 44);
    assert_eq!(limited_ids, full_ids[..2]);

    let full_stage = &full["stages"][1];
    let limited_stage = &limited["stages"][1];
    assert_eq!(full_stage["edges"], json!(44));
    assert_eq!(limited_stage["edges"], json!(2), "stopped after reading just 2 edges: {limited_stage}");
    assert_eq!(limited_stage["kept"], json!(2));
    assert!(
        limited_stage["visited"].as_u64().unwrap() < full_stage["visited"].as_u64().unwrap(),
        "limited visited should be much smaller: {limited_stage} vs {full_stage}"
    );

    // Filtered by type `Note`: must traverse through `Bridge-*` intermediates and stop after yielding 3 Notes
    let full_notes = read_explained(
        &db,
        json!({
            "start": { "from": "ids", "ids": ["Note-0"] },
            "steps": [{ "step": "descendants", "kinds": ["child"], "types": ["Note"] }],
            "terminal": { "answer": "ids" }
        }),
    );
    let limited_notes = read_explained(
        &db,
        json!({
            "start": { "from": "ids", "ids": ["Note-0"] },
            "steps": [
                { "step": "descendants", "kinds": ["child"], "types": ["Note"] },
                { "step": "limit", "n": 3 }
            ],
            "terminal": { "answer": "ids" }
        }),
    );
    let full_note_ids: Vec<String> = serde_json::from_value(full_notes["answer"]["ids"].clone()).unwrap();
    let limited_note_ids: Vec<String> = serde_json::from_value(limited_notes["answer"]["ids"].clone()).unwrap();
    assert_eq!(full_note_ids.len(), 40);
    assert_eq!(limited_note_ids, full_note_ids[..3]);
    assert_eq!(limited_notes["stages"][1]["edges"], json!(7), "4 level-1 edges + 3 level-2 edges");
    assert_eq!(limited_notes["stages"][1]["kept"], json!(3));

    // Ancestors with limit 1 from a leaf stops after 1 edge
    let limited_up = read_explained(
        &db,
        json!({
            "start": { "from": "ids", "ids": ["Note-1-0"] },
            "steps": [
                { "step": "ancestors", "kinds": ["child"] },
                { "step": "limit", "n": 1 }
            ],
            "terminal": { "answer": "ids" }
        }),
    );
    assert_eq!(limited_up["answer"]["ids"], json!(["Bridge-1"]));
    assert_eq!(limited_up["stages"][1]["edges"], json!(1));
    assert_eq!(limited_up["stages"][1]["kept"], json!(1));

    // Depth 0 walks 0 edges
    let zero_depth = read_explained(
        &db,
        json!({
            "start": { "from": "ids", "ids": ["Note-0"] },
            "steps": [{ "step": "descendants", "kinds": ["child"], "depth": 0 }],
            "terminal": { "answer": "ids" }
        }),
    );
    assert_eq!(zero_depth["answer"]["ids"], json!([]));
    assert_eq!(zero_depth["stages"][1]["edges"], json!(0));
    assert_eq!(zero_depth["stages"][1]["kept"], json!(0));
}

#[test]
fn explain_telemetry_covers_trigram_vector_and_graph_stages_without_payloads() {
    let dir = tempfile::tempdir().unwrap();
    let db = open_db(&dir);

    let secret_a = "SENTINEL_ALPHA_PAYLOAD_8192";
    let secret_b = "SENTINEL_BETA_PAYLOAD_4096";
    let secret_c = "SENTINEL_GAMMA_PAYLOAD_2048";

    write_ops(
        &db,
        vec![
            json!({ "op": "create", "type": "Note", "id": "Note-alpha", "fields": { "title": "rust engine", "body": secret_a } }),
            json!({ "op": "create", "type": "Note", "id": "Note-beta", "fields": { "title": "vector graph", "body": secret_b } }),
            json!({ "op": "create", "type": "Note", "id": "Note-gamma", "fields": { "title": "ab short", "body": secret_c } }),
            json!({ "op": "link", "source": "Note-alpha", "kind": "refs", "target": "Note-beta" }),
            json!({ "op": "link", "source": "Note-beta", "kind": "refs", "target": "Note-gamma" }),
        ],
    );
    db.wait_for_index_builds().unwrap();

    // Embed the first two notes (leave Note-gamma unembedded for Like empty-vector coverage)
    let pending_reads: Reads =
        serde_json::from_value(json!({ "reads": [{ "read": "pendingEmbeddings", "type": "Note" }] })).unwrap();
    let pending_bytes = db.read(&pending_reads).unwrap();
    let pending = serde_json::to_value(rmpv::decode::read_value(&mut &pending_bytes[..]).unwrap()).unwrap();
    let mut embed_ops = Vec::new();
    for chunk in pending[0]["chunks"].as_array().unwrap() {
        let id = chunk["id"].as_str().unwrap();
        let vec = match id {
            "Note-alpha" => [1.0f32, 0.0, 0.0],
            "Note-beta" => [0.8f32, 0.6, 0.0],
            _ => continue,
        };
        embed_ops.push(json!({
            "op": "embed",
            "id": id,
            "index": "meaning",
            "chunk": chunk["chunk"],
            "hash": chunk["hash"],
            "model": "m1",
            "vector": vec
        }));
    }
    write_ops(&db, embed_ops);

    let assert_payload_free = |explained: &Json, label: &str| {
        let stages_json = serde_json::to_string(&explained["stages"]).unwrap();
        for forbidden in [secret_a, secret_b, secret_c, "Note-alpha", "Note-beta", "Note-gamma", "rust engine"] {
            assert!(!stages_json.contains(forbidden), "{label}: stages leaked {forbidden:?}: {stages_json}");
        }
    };

    // 1. Containing with trigram narrowing
    let cont_indexed = read_explained(
        &db,
        json!({
            "start": { "from": "types", "types": ["Note"] },
            "steps": [{ "step": "containing", "text": "engine" }],
            "terminal": { "answer": "ids" }
        }),
    );
    assert_eq!(cont_indexed["stages"][1]["index"], json!("grams"));
    assert_eq!(cont_indexed["stages"][1]["candidates"], json!(1));
    assert_eq!(cont_indexed["stages"][1]["kept"], json!(1));
    assert_payload_free(&cont_indexed, "containing indexed");

    // 2. Containing with full-scan fallback (substring shorter than 3 chars)
    let cont_scan = read_explained(
        &db,
        json!({
            "start": { "from": "types", "types": ["Note"] },
            "steps": [{ "step": "containing", "text": "ab" }],
            "terminal": { "answer": "ids" }
        }),
    );
    assert_eq!(cont_scan["stages"][1]["index"], json!("none: walks the type"));
    assert!(cont_scan["stages"][1].get("candidates").is_none());
    assert_eq!(cont_scan["stages"][1]["visited"], json!(3));
    assert_eq!(cont_scan["stages"][1]["kept"], json!(1));
    assert_payload_free(&cont_scan, "containing scan");

    // 3. Matching with trigram narrowing vs full-scan fallback
    let match_indexed = read_explained(
        &db,
        json!({
            "start": { "from": "types", "types": ["Note"] },
            "steps": [{ "step": "matching", "pattern": "vector.*graph" }],
            "terminal": { "answer": "ids" }
        }),
    );
    assert_eq!(match_indexed["stages"][1]["index"], json!("grams"));
    assert_eq!(match_indexed["stages"][1]["candidates"], json!(1));
    assert_payload_free(&match_indexed, "matching indexed");

    let match_scan = read_explained(
        &db,
        json!({
            "start": { "from": "types", "types": ["Note"] },
            "steps": [{ "step": "matching", "pattern": "[a-z]+" }],
            "terminal": { "answer": "ids" }
        }),
    );
    assert_eq!(match_scan["stages"][1]["index"], json!("none: walks the type"));
    assert!(match_scan["stages"][1].get("candidates").is_none());
    assert_payload_free(&match_scan, "matching scan");

    // 4. Near
    let near = read_explained(
        &db,
        json!({
            "start": { "from": "types", "types": ["Note"] },
            "steps": [{ "step": "near", "vector": [1.0, 0.0, 0.0], "k": 2 }],
            "terminal": { "answer": "ids" }
        }),
    );
    assert_eq!(near["stages"][1]["index"], json!("meaning"));
    assert_eq!(near["stages"][1]["neighbours"], json!(2));
    assert_eq!(near["stages"][1]["candidates"], json!(2));
    assert_eq!(near["stages"][1]["kept"], json!(2));
    assert_payload_free(&near, "near");

    // 5. Like (embedded and unembedded)
    let like = read_explained(
        &db,
        json!({
            "start": { "from": "types", "types": ["Note"] },
            "steps": [{ "step": "like", "id": "Note-alpha", "k": 2 }],
            "terminal": { "answer": "ids" }
        }),
    );
    assert_eq!(like["stages"][1]["index"], json!("meaning"));
    assert_eq!(like["stages"][1]["neighbours"], json!(2));
    assert_eq!(like["stages"][1]["candidates"], json!(1));
    assert_eq!(like["stages"][1]["kept"], json!(1));
    assert_payload_free(&like, "like");

    let like_missing = read_explained(
        &db,
        json!({
            "start": { "from": "types", "types": ["Note"] },
            "steps": [{ "step": "like", "id": "Note-gamma", "k": 2 }],
            "terminal": { "answer": "ids" }
        }),
    );
    assert_eq!(like_missing["stages"][1]["index"], json!("meaning"));
    assert_eq!(like_missing["stages"][1]["kept"], json!(0));
    assert_payload_free(&like_missing, "like missing");

    // 6. Hybrid
    let hybrid = read_explained(
        &db,
        json!({
            "start": { "from": "types", "types": ["Note"] },
            "steps": [{ "step": "hybrid", "query": "rust", "vector": [1.0, 0.0, 0.0], "k": 2 }],
            "terminal": { "answer": "ids" }
        }),
    );
    assert_eq!(hybrid["stages"][1]["index"], json!("meaning, text"));
    assert_eq!(hybrid["stages"][1]["neighbours"], json!(2));
    assert_eq!(hybrid["stages"][1]["candidates"], json!(3), "2 vector hits + 1 text hit");
    assert_eq!(hybrid["stages"][1]["kept"], json!(2));
    assert_payload_free(&hybrid, "hybrid");

    // 7. PathTo (found, self-start, missing target)
    let path_found = read_explained(
        &db,
        json!({
            "start": { "from": "ids", "ids": ["Note-alpha"] },
            "steps": [{ "step": "pathTo", "to": "Note-gamma", "kinds": ["refs"] }],
            "terminal": { "answer": "ids" }
        }),
    );
    assert_eq!(path_found["answer"]["ids"], json!(["Note-alpha", "Note-beta", "Note-gamma"]));
    assert_eq!(path_found["stages"][1]["edges"], json!(2));
    assert_eq!(path_found["stages"][1]["visited"], json!(3), "1 received + 2 BFS nodes checked");
    assert_eq!(path_found["stages"][1]["kept"], json!(3));
    assert_payload_free(&path_found, "pathTo found");

    let path_self = read_explained(
        &db,
        json!({
            "start": { "from": "ids", "ids": ["Note-alpha"] },
            "steps": [{ "step": "pathTo", "to": "Note-alpha", "kinds": ["refs"] }],
            "terminal": { "answer": "ids" }
        }),
    );
    assert_eq!(path_self["stages"][1]["edges"], json!(0));
    assert_eq!(path_self["stages"][1]["kept"], json!(1));
    assert_payload_free(&path_self, "pathTo self");

    let path_none = read_explained(
        &db,
        json!({
            "start": { "from": "ids", "ids": ["Note-alpha"] },
            "steps": [{ "step": "pathTo", "to": "Note-missing", "kinds": ["refs"] }],
            "terminal": { "answer": "ids" }
        }),
    );
    assert_eq!(path_none["stages"][1]["edges"], json!(0));
    assert_eq!(path_none["stages"][1]["kept"], json!(0));
    assert_payload_free(&path_none, "pathTo none");

    // Alive target with no outgoing path walks available edges and records them
    let path_unreachable = read_explained(
        &db,
        json!({
            "start": { "from": "ids", "ids": ["Note-beta"] },
            "steps": [{ "step": "pathTo", "to": "Note-alpha", "kinds": ["refs"], "direction": "out" }],
            "terminal": { "answer": "ids" }
        }),
    );
    assert_eq!(path_unreachable["answer"]["ids"], json!([]));
    assert_eq!(path_unreachable["stages"][1]["edges"], json!(1));
    assert_eq!(path_unreachable["stages"][1]["kept"], json!(0));
    assert_payload_free(&path_unreachable, "pathTo unreachable");
}
