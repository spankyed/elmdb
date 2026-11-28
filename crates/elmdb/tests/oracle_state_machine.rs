//! Shrinking `proptest-state-machine` oracle on real `Database` + approximate-index rebuild &
//! recall-bound oracle.
//!
//! 1. `database_deterministic_state_machine_oracle`:
//!    - Drives real `Database` (`LmdbStore` + `OsVfs`) against an in-memory reference model across
//!      CRUD (`Create`, `Patch`, `Unset`, `Trash`, `Restore`, `Destroy`), `Link`/`Unlink`, nested
//!      savepoints (`transact_scoped` inner rollback + outer commit, and outer rollback),
//!      `beforeCommit` host hook cascades, O(1) `rename_field`, archive + `purge_archived`,
//!      streaming `dump`/`load` round-trips, `check(None)`, and all 5 deterministic index kinds
//!      (`Value`, `Unique`, `Order`, `Text`, `Trigram`) — asserting exact state AND exact refusal
//!      `ErrorCode`s (`EntityAlreadyExists`, `EntityNotFound`, `EntityTrashed`, `EntityNotTrashed`,
//!      `UniqueViolation`).
//!    - Includes a step-coverage guard proving every transition kind and key refusal path executed.
//! 2. `approximate_indexes_match_rebuild_and_recall_bounds`:
//!    - Exercises `Vector` (HNSW SQ8), `Rank` (PageRank), `GraphPpr`, and `GraphLeiden` across
//!      randomized mutations, proving `check(None).clean`, `derived == rebuild(primary)` within
//!      strict tolerance (`<= 1e-4`), and HNSW recall@5 `>= 0.85` against brute-force cosine.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, LazyLock, Mutex};

use elmdb::protocol::{
    QueryAnswerBody, QueryFilter, QueryScore, QuerySource, QuerySpec, QueryStep, QueryTerminal, Schema,
};
use elmdb::{
    BuildPolicy, CanonicalValue, CommitReturnFlags, Database, DurabilityMode, EdgeDir, EntityTarget, Error, ErrorCode,
    ExecBudgets, OpenOptions, TransactOptions, TypeMask, WireOp, encode_batch,
};
use proptest::prelude::*;
use proptest_state_machine::{ReferenceStateMachine, StateMachineTest, prop_state_machine};

const ID_POOL: [&str; 6] = ["n0", "n1", "n2", "n3", "n4", "n5"];
const STATUS_POOL: [&str; 3] = ["open", "done", "audited"];
const SLUG_POOL: [&str; 4] = ["s-alpha", "s-beta", "s-gamma", "s-delta"];
const TITLE_POOL: [&str; 4] = ["alpha project", "beta roadmap", "gamma engine", "delta notes"];
const LABEL_POOL: [&str; 4] = ["apple", "banana", "cherry", "date"];
const BODY_POOL: [&str; 4] = [
    "rust storage engine btree",
    "lmdb mvcc durable snapshot",
    "query planner trigram index",
    "graph analytics pagerank community",
];

static COVERED_STEPS: LazyLock<Mutex<BTreeSet<&'static str>>> = LazyLock::new(|| Mutex::new(BTreeSet::new()));

fn record_step(name: &'static str) {
    COVERED_STEPS.lock().unwrap().insert(name);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExtraFieldState {
    Active(&'static str),
    Archived(&'static str),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct OracleRow {
    trashed: bool,
    title: Option<String>,
    status: Option<String>,
    slug: Option<String>,
    label: Option<String>,
    body: Option<String>,
    extra: Option<String>,
}

#[derive(Debug, Clone)]
struct OracleRefState {
    rows: BTreeMap<String, OracleRow>,
    audits: BTreeSet<String>,
    edges: BTreeSet<(String, String)>,
    extra_state: ExtraFieldState,
    scratch_active: bool,
    scratch_ids: BTreeSet<String>,
    audit_seq: u64,
}

impl OracleRefState {
    fn new() -> Self {
        Self {
            rows: BTreeMap::new(),
            audits: BTreeSet::new(),
            edges: BTreeSet::new(),
            extra_state: ExtraFieldState::Active("extra"),
            scratch_active: true,
            scratch_ids: BTreeSet::new(),
            audit_seq: 0,
        }
    }

    fn slug_owner(&self, slug: &str) -> Option<&str> {
        self.rows.iter().find(|(_, r)| r.slug.as_deref() == Some(slug)).map(|(id, _)| id.as_str())
    }
}

#[derive(Debug, Clone)]
enum Transition {
    Create {
        id_idx: usize,
        title_idx: usize,
        status_idx: usize,
        slug_idx: Option<usize>,
        label_idx: usize,
        body_idx: usize,
        extra: Option<String>,
    },
    Patch {
        id_idx: usize,
        status_idx: usize,
        slug_idx: Option<usize>,
        body_idx: usize,
        extra: Option<String>,
    },
    Unset {
        id_idx: usize,
        clear_slug: bool,
        clear_extra: bool,
    },
    Trash {
        id_idx: usize,
    },
    Restore {
        id_idx: usize,
    },
    Destroy {
        id_idx: usize,
    },
    Link {
        src_idx: usize,
        dst_idx: usize,
    },
    Unlink {
        src_idx: usize,
        dst_idx: usize,
    },
    NestedSavepoint {
        keep_idx: usize,
        abort_idx: usize,
    },
    OuterRollback {
        abort_idx: usize,
    },
    RenameExtraField,
    ArchiveAndPurgeExtra,
    DumpLoadVerify,
    CheckInvariants,
}

fn oracle_schema(extra_field: Option<&str>, include_scratch: bool) -> Schema {
    let mut note_fields = serde_json::json!({
        "title": { "type": "string", "optional": true },
        "status": { "type": "string", "optional": true },
        "slug": { "type": "string", "optional": true },
        "label": { "type": "string", "optional": true },
        "body": { "type": "string", "optional": true }
    });
    if let Some(name) = extra_field {
        note_fields[name] = serde_json::json!({ "type": "string", "optional": true });
    }
    let mut types = serde_json::json!({
        "Note": {
            "fields": note_fields,
            "indexes": {
                "by_status": { "kind": "value", "fields": ["status"] },
                "by_slug": { "kind": "value", "fields": ["slug"], "unique": true },
                "by_label": { "kind": "order", "field": "label", "locale": "en" },
                "by_body": { "kind": "text", "fields": [["body", 1]] },
                "by_title": { "kind": "trigram", "fields": ["title"] }
            }
        },
        "Audit": {
            "fields": {
                "noteId": { "type": "string" }
            }
        }
    });
    if include_scratch {
        types["Scratch"] = serde_json::json!({
            "fields": {
                "tag": { "type": "string", "optional": true }
            }
        });
    }
    serde_json::from_value(serde_json::json!({
        "types": types,
        "relations": {
            "refs": {
                "from": ["Note"],
                "to": ["Note"]
            }
        }
    }))
    .unwrap()
}

fn transition_strategy() -> BoxedStrategy<Transition> {
    prop_oneof![
        4 => (
            0..ID_POOL.len(),
            0..TITLE_POOL.len(),
            0..STATUS_POOL.len(),
            prop::option::of(0..SLUG_POOL.len()),
            0..LABEL_POOL.len(),
            0..BODY_POOL.len(),
            prop::option::of(prop_oneof![Just("e1".to_owned()), Just("e2".to_owned())]),
        )
            .prop_map(|(id_idx, title_idx, status_idx, slug_idx, label_idx, body_idx, extra)| {
                Transition::Create { id_idx, title_idx, status_idx, slug_idx, label_idx, body_idx, extra }
            }),
        3 => (
            0..ID_POOL.len(),
            0..STATUS_POOL.len(),
            prop::option::of(0..SLUG_POOL.len()),
            0..BODY_POOL.len(),
            prop::option::of(prop_oneof![Just("p1".to_owned()), Just("p2".to_owned())]),
        )
            .prop_map(|(id_idx, status_idx, slug_idx, body_idx, extra)| Transition::Patch {
                id_idx,
                status_idx,
                slug_idx,
                body_idx,
                extra,
            }),
        2 => (0..ID_POOL.len(), any::<bool>(), any::<bool>())
            .prop_map(|(id_idx, clear_slug, clear_extra)| Transition::Unset { id_idx, clear_slug, clear_extra }),
        2 => (0..ID_POOL.len()).prop_map(|id_idx| Transition::Trash { id_idx }),
        2 => (0..ID_POOL.len()).prop_map(|id_idx| Transition::Restore { id_idx }),
        2 => (0..ID_POOL.len()).prop_map(|id_idx| Transition::Destroy { id_idx }),
        3 => (0..ID_POOL.len(), 0..ID_POOL.len()).prop_map(|(src_idx, dst_idx)| Transition::Link { src_idx, dst_idx }),
        2 => (0..ID_POOL.len(), 0..ID_POOL.len())
            .prop_map(|(src_idx, dst_idx)| Transition::Unlink { src_idx, dst_idx }),
        2 => (0..ID_POOL.len(), 0..ID_POOL.len())
            .prop_map(|(keep_idx, abort_idx)| Transition::NestedSavepoint { keep_idx, abort_idx }),
        1 => (0..ID_POOL.len()).prop_map(|abort_idx| Transition::OuterRollback { abort_idx }),
        1 => Just(Transition::RenameExtraField),
        1 => Just(Transition::ArchiveAndPurgeExtra),
        1 => Just(Transition::DumpLoadVerify),
        2 => Just(Transition::CheckInvariants),
    ]
    .boxed()
}

struct OracleRefMachine;

impl ReferenceStateMachine for OracleRefMachine {
    type State = OracleRefState;
    type Transition = Transition;

    fn init_state() -> BoxedStrategy<Self::State> {
        Just(OracleRefState::new()).boxed()
    }

    fn transitions(_state: &Self::State) -> BoxedStrategy<Self::Transition> {
        transition_strategy()
    }

    fn apply(mut state: Self::State, transition: &Self::Transition) -> Self::State {
        match transition {
            Transition::Create { id_idx, title_idx, status_idx, slug_idx, label_idx, body_idx, extra } => {
                let id = ID_POOL[*id_idx];
                if state.rows.contains_key(id) {
                    return state;
                }
                let slug = slug_idx.map(|i| SLUG_POOL[i].to_owned());
                if let Some(ref s) = slug
                    && state.slug_owner(s).is_some()
                {
                    return state;
                }
                let status = STATUS_POOL[*status_idx].to_owned();
                let stored_extra = match state.extra_state {
                    ExtraFieldState::Active(_) => extra.clone(),
                    ExtraFieldState::Archived(_) => None,
                };
                state.rows.insert(
                    id.to_owned(),
                    OracleRow {
                        trashed: false,
                        title: Some(TITLE_POOL[*title_idx].to_owned()),
                        status: Some(status.clone()),
                        slug,
                        label: Some(LABEL_POOL[*label_idx].to_owned()),
                        body: Some(BODY_POOL[*body_idx].to_owned()),
                        extra: stored_extra,
                    },
                );
                if status == "audited" {
                    state.audit_seq += 1;
                    state.audits.insert(format!("audit-{id}-{}", state.audit_seq));
                }
            }
            Transition::Patch { id_idx, status_idx, slug_idx, body_idx, extra } => {
                let id = ID_POOL[*id_idx];
                let Some(existing) = state.rows.get(id) else {
                    return state;
                };
                if existing.trashed {
                    return state;
                }
                let new_slug = slug_idx.map(|i| SLUG_POOL[i].to_owned());
                if let Some(ref s) = new_slug
                    && let Some(owner) = state.slug_owner(s)
                    && owner != id
                {
                    return state;
                }
                let row = state.rows.get_mut(id).unwrap();
                row.status = Some(STATUS_POOL[*status_idx].to_owned());
                if let Some(s) = new_slug {
                    row.slug = Some(s);
                }
                row.body = Some(BODY_POOL[*body_idx].to_owned());
                if matches!(state.extra_state, ExtraFieldState::Active(_))
                    && let Some(e) = extra
                {
                    row.extra = Some(e.clone());
                }
            }
            Transition::Unset { id_idx, clear_slug, clear_extra } => {
                let id = ID_POOL[*id_idx];
                if let Some(row) = state.rows.get_mut(id) {
                    if row.trashed {
                        return state;
                    }
                    if *clear_slug {
                        row.slug = None;
                    }
                    if *clear_extra && matches!(state.extra_state, ExtraFieldState::Active(_)) {
                        row.extra = None;
                    }
                }
            }
            Transition::Trash { id_idx } => {
                let id = ID_POOL[*id_idx];
                if let Some(row) = state.rows.get_mut(id)
                    && !row.trashed
                {
                    row.trashed = true;
                }
            }
            Transition::Restore { id_idx } => {
                let id = ID_POOL[*id_idx];
                if let Some(row) = state.rows.get_mut(id)
                    && row.trashed
                {
                    row.trashed = false;
                }
            }
            Transition::Destroy { id_idx } => {
                let id = ID_POOL[*id_idx];
                if state.rows.remove(id).is_some() {
                    state.edges.retain(|(s, d)| s != id && d != id);
                }
            }
            Transition::Link { src_idx, dst_idx } => {
                let src = ID_POOL[*src_idx];
                let dst = ID_POOL[*dst_idx];
                let ok_src = state.rows.get(src).is_some_and(|r| !r.trashed);
                let ok_dst = state.rows.get(dst).is_some_and(|r| !r.trashed);
                if ok_src && ok_dst {
                    state.edges.insert((src.to_owned(), dst.to_owned()));
                }
            }
            Transition::Unlink { src_idx, dst_idx } => {
                let src = ID_POOL[*src_idx];
                let dst = ID_POOL[*dst_idx];
                state.edges.remove(&(src.to_owned(), dst.to_owned()));
            }
            Transition::NestedSavepoint { keep_idx, abort_idx: _ } => {
                let keep_id = ID_POOL[*keep_idx];
                if !state.rows.contains_key(keep_id) {
                    state.rows.insert(
                        keep_id.to_owned(),
                        OracleRow {
                            trashed: false,
                            title: Some(TITLE_POOL[0].to_owned()),
                            status: Some("audited".to_owned()),
                            slug: None,
                            label: Some(LABEL_POOL[0].to_owned()),
                            body: Some(BODY_POOL[0].to_owned()),
                            extra: None,
                        },
                    );
                    state.audit_seq += 1;
                    state.audits.insert(format!("audit-{keep_id}-{}", state.audit_seq));
                }
            }
            Transition::OuterRollback { .. } => {}
            Transition::RenameExtraField => {
                if state.extra_state == ExtraFieldState::Active("extra") {
                    state.extra_state = ExtraFieldState::Active("memo");
                }
            }
            Transition::ArchiveAndPurgeExtra => {
                if let ExtraFieldState::Active(name) = state.extra_state {
                    state.extra_state = ExtraFieldState::Archived(name);
                    state.scratch_active = false;
                    state.scratch_ids.clear();
                }
            }
            Transition::DumpLoadVerify | Transition::CheckInvariants => {}
        }
        state
    }
}

struct OracleSut {
    _dir: tempfile::TempDir,
    db: Arc<Database>,
    audit_counter: Arc<Mutex<u64>>,
    /// Pre-transition reference snapshot maintained inside the SUT because `proptest-state-machine`
    /// passes the *post*-transition `ref_state` into `StateMachineTest::apply`.
    pre: OracleRefState,
}

impl OracleSut {
    fn verify_rows_and_indexes(&self, ref_state: &OracleRefState) {
        // 1. Structural integrity check must always be clean.
        let report = self.db.check(None).expect("check(None)");
        assert!(report.clean, "check(None) reported issues: {report:?}");

        // 2. Exact row-by-row comparison for all Note IDs (using `get_row_resolved` for boundary view).
        for id in ID_POOL {
            let actual = self.db.get_row_resolved(id).unwrap();
            match ref_state.rows.get(id) {
                None => assert!(actual.is_none(), "expected {id} absent, got {actual:?}"),
                Some(exp) => {
                    let row = actual.unwrap_or_else(|| panic!("expected {id} present"));
                    assert_eq!(row.trashed, exp.trashed, "trashed mismatch on {id}");
                    assert_eq!(row.fields.get("title").and_then(|v| v.as_str()), exp.title.as_deref());
                    assert_eq!(row.fields.get("status").and_then(|v| v.as_str()), exp.status.as_deref());
                    assert_eq!(row.fields.get("slug").and_then(|v| v.as_str()), exp.slug.as_deref());
                    assert_eq!(row.fields.get("label").and_then(|v| v.as_str()), exp.label.as_deref());
                    assert_eq!(row.fields.get("body").and_then(|v| v.as_str()), exp.body.as_deref());
                    match ref_state.extra_state {
                        ExtraFieldState::Active(field_name) | ExtraFieldState::Archived(field_name) => {
                            assert_eq!(row.fields.get(field_name).and_then(|v| v.as_str()), exp.extra.as_deref());
                        }
                    }
                }
            }
        }

        // 3. Exact Audit rows created by beforeCommit hook.
        let audit_spec = QuerySpec {
            source: QuerySource::Type { type_name: "Audit".into(), include_trashed: false },
            steps: vec![],
            terminal: Some(QueryTerminal::Ids),
            explain: false,
        };
        let actual_audits = ids_from_answer(&self.db.execute_query_spec(&audit_spec, ExecBudgets::default()).unwrap());
        assert_eq!(actual_audits, ref_state.audits, "hook-created Audit set mismatch");

        // 4. Exact relation edge set.
        let mut actual_edges = BTreeSet::new();
        for id in ID_POOL {
            if ref_state.rows.contains_key(id) {
                for rel in self.db.get_edges_resolved(id, Some("refs"), EdgeDir::Out).unwrap() {
                    actual_edges.insert((rel.source_id, rel.target_id));
                }
            }
        }
        assert_eq!(actual_edges, ref_state.edges, "relation edge set mismatch");

        // 5. Value index (`by_status`) exact match on live (non-trashed) rows.
        for status in STATUS_POOL {
            let spec = QuerySpec {
                source: QuerySource::Type { type_name: "Note".into(), include_trashed: false },
                steps: vec![QueryStep::Filter {
                    expr: QueryFilter::Eq { field: "status".into(), value: serde_json::json!(status) },
                }],
                terminal: Some(QueryTerminal::Ids),
                explain: false,
            };
            let got = ids_from_answer(&self.db.execute_query_spec(&spec, ExecBudgets::default()).unwrap());
            let expected: BTreeSet<String> = ref_state
                .rows
                .iter()
                .filter(|(_, r)| !r.trashed && r.status.as_deref() == Some(status))
                .map(|(id, _)| id.clone())
                .collect();
            assert_eq!(got, expected, "Value index mismatch for status={status}");
        }

        // 6. Unique index (`by_slug`) exact match (including trashed=false).
        for slug in SLUG_POOL {
            let spec = QuerySpec {
                source: QuerySource::Type { type_name: "Note".into(), include_trashed: false },
                steps: vec![QueryStep::Filter {
                    expr: QueryFilter::Eq { field: "slug".into(), value: serde_json::json!(slug) },
                }],
                terminal: Some(QueryTerminal::Ids),
                explain: false,
            };
            let got = ids_from_answer(&self.db.execute_query_spec(&spec, ExecBudgets::default()).unwrap());
            let expected: BTreeSet<String> = ref_state
                .rows
                .iter()
                .filter(|(_, r)| !r.trashed && r.slug.as_deref() == Some(slug))
                .map(|(id, _)| id.clone())
                .collect();
            assert_eq!(got, expected, "Unique index mismatch for slug={slug}");
        }

        // 7. Order index (`by_label`, asc) exact ordering match.
        {
            let spec = QuerySpec {
                source: QuerySource::Type { type_name: "Note".into(), include_trashed: false },
                steps: vec![QueryStep::OrderSlice {
                    by: "field".into(),
                    field: Some("label".into()),
                    dir: "asc".into(),
                    locale: Some("en".into()),
                    after_id: None,
                    after_score: None,
                    after_value: None,
                    limit: Some(20),
                }],
                terminal: Some(QueryTerminal::Ids),
                explain: false,
            };
            let got_vec = ids_vec_from_answer(&self.db.execute_query_spec(&spec, ExecBudgets::default()).unwrap());
            let got_labels: Vec<Option<&str>> = got_vec.iter().map(|id| ref_state.rows[id].label.as_deref()).collect();
            let mut expected_labels: Vec<Option<&str>> =
                ref_state.rows.values().filter(|r| !r.trashed).map(|r| r.label.as_deref()).collect();
            expected_labels.sort();
            assert_eq!(got_labels, expected_labels, "Order index label ordering mismatch");
        }

        // 8. Text index (`by_body` BM25F) exact matching set for distinctive token `"lmdb"`.
        {
            let spec = QuerySpec {
                source: QuerySource::Type { type_name: "Note".into(), include_trashed: false },
                steps: vec![QueryStep::Score {
                    spec: QueryScore::Bm25f {
                        index: "by_body".into(),
                        query: "lmdb".into(),
                        require_all: true,
                        prefix: false,
                        phrase: None,
                        top_k: Some(20),
                    },
                }],
                terminal: Some(QueryTerminal::Ids),
                explain: false,
            };
            let got = ids_from_answer(&self.db.execute_query_spec(&spec, ExecBudgets::default()).unwrap());
            let expected: BTreeSet<String> = ref_state
                .rows
                .iter()
                .filter(|(_, r)| !r.trashed && r.body.as_deref().is_some_and(|b| b.contains("lmdb")))
                .map(|(id, _)| id.clone())
                .collect();
            assert_eq!(got, expected, "Text index BM25F mismatch for 'lmdb'");
        }

        // 9. Trigram index (`by_title` Contains `"road"`) exact matching set.
        {
            let spec = QuerySpec {
                source: QuerySource::Type { type_name: "Note".into(), include_trashed: false },
                steps: vec![QueryStep::Filter {
                    expr: QueryFilter::Contains { field: "title".into(), needle: "road".into() },
                }],
                terminal: Some(QueryTerminal::Ids),
                explain: false,
            };
            let got = ids_from_answer(&self.db.execute_query_spec(&spec, ExecBudgets::default()).unwrap());
            let expected: BTreeSet<String> = ref_state
                .rows
                .iter()
                .filter(|(_, r)| !r.trashed && r.title.as_deref().is_some_and(|t| t.contains("road")))
                .map(|(id, _)| id.clone())
                .collect();
            assert_eq!(got, expected, "Trigram index Contains mismatch for 'road'");
        }
    }
}

fn ids_vec_from_answer(ans: &elmdb::protocol::QueryAnswer) -> Vec<String> {
    match &ans.body {
        QueryAnswerBody::Ids { ids } => ids.clone(),
        QueryAnswerBody::Rows { rows } => rows.iter().map(|r| r.id.clone()).collect(),
        QueryAnswerBody::Hits { hits } => hits.iter().map(|h| h.id.clone()).collect(),
        other => panic!("unexpected QueryAnswerBody: {other:?}"),
    }
}

fn ids_from_answer(ans: &elmdb::protocol::QueryAnswer) -> BTreeSet<String> {
    ids_vec_from_answer(ans).into_iter().collect()
}

impl StateMachineTest for OracleSut {
    type SystemUnderTest = Self;
    type Reference = OracleRefMachine;

    fn init_test(ref_state: &OracleRefState) -> Self::SystemUnderTest {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(
            Database::open(
                dir.path(),
                OpenOptions {
                    durability: DurabilityMode::Grouped,
                    build_policy: BuildPolicy::EagerInSchemaChange,
                    map_size: Some(8 * 1024 * 1024),
                    ..Default::default()
                },
            )
            .unwrap(),
        );
        db.set_schema(&oracle_schema(Some("extra"), true)).unwrap();
        // Seed one Scratch entity so `ArchiveAndPurgeExtra` always exercises real `purge_archived("Scratch")`.
        db.execute_batch(&[WireOp::Create {
            type_name: "Scratch".into(),
            external_id: Some("scratch-0".into()),
            fields: vec![("tag".into(), CanonicalValue::String("temp".into()))],
        }])
        .unwrap();

        // Register a deterministic host `beforeCommit` hook: whenever a `Note` is created with
        // `status == "audited"`, cascade-create an `Audit` row inside the same atomic commit!
        let audit_counter = Arc::new(Mutex::new(0u64));
        let db_hook = Arc::clone(&db);
        let counter_hook = Arc::clone(&audit_counter);
        db.register_host_before_commit(
            TypeMask::all(),
            Arc::new(move |delta| {
                let mut ops = Vec::new();
                for e in &delta.entities {
                    if e.type_name == "Note"
                        && e.op == "create"
                        && let Some(row) = db_hook.get_row(&e.id)?
                        && row.fields.get("status").and_then(|v| v.as_str()) == Some("audited")
                    {
                        let mut c = counter_hook.lock().unwrap();
                        *c += 1;
                        let audit_id = format!("audit-{}-{}", e.id, *c);
                        ops.push(WireOp::Create {
                            type_name: "Audit".into(),
                            external_id: Some(audit_id),
                            fields: vec![("noteId".into(), CanonicalValue::String(e.id.clone()))],
                        });
                    }
                }
                if ops.is_empty() { Ok(None) } else { Ok(Some(encode_batch(CommitReturnFlags::NONE, &ops))) }
            }),
        )
        .unwrap();

        Self { _dir: dir, db, audit_counter, pre: ref_state.clone() }
    }

    fn apply(
        mut state: Self::SystemUnderTest,
        post_ref: &OracleRefState,
        transition: Transition,
    ) -> Self::SystemUnderTest {
        let pre = state.pre.clone();
        match transition {
            Transition::Create { id_idx, title_idx, status_idx, slug_idx, label_idx, body_idx, extra } => {
                record_step("create");
                let id = ID_POOL[id_idx];
                let mut fields = vec![
                    ("title".into(), CanonicalValue::String(TITLE_POOL[title_idx].into())),
                    ("status".into(), CanonicalValue::String(STATUS_POOL[status_idx].into())),
                    ("label".into(), CanonicalValue::String(LABEL_POOL[label_idx].into())),
                    ("body".into(), CanonicalValue::String(BODY_POOL[body_idx].into())),
                ];
                let slug = slug_idx.map(|i| SLUG_POOL[i]);
                if let Some(s) = slug {
                    fields.push(("slug".into(), CanonicalValue::String(s.into())));
                }
                if let ExtraFieldState::Active(fname) = pre.extra_state
                    && let Some(e) = extra
                {
                    fields.push((fname.into(), CanonicalValue::String(e)));
                }
                // Snapshot counter before attempt so hook rollback keeps counter synchronized with model.
                let counter_before = *state.audit_counter.lock().unwrap();
                let res = state.db.execute_batch(&[WireOp::Create {
                    type_name: "Note".into(),
                    external_id: Some(id.into()),
                    fields,
                }]);
                // Compare exact success vs exact refusal ErrorCode against pre-transition state!
                if pre.rows.contains_key(id) {
                    let err = res.unwrap_err();
                    assert_eq!(err.code, ErrorCode::EntityAlreadyExists, "duplicate create {id}: {err:?}");
                    record_step("refusal_already_exists");
                    *state.audit_counter.lock().unwrap() = counter_before;
                } else if slug.is_some_and(|s| pre.slug_owner(s).is_some()) {
                    let err = res.unwrap_err();
                    assert_eq!(err.code, ErrorCode::UniqueViolation, "unique slug collision on {id}: {err:?}");
                    record_step("refusal_unique_violation");
                    *state.audit_counter.lock().unwrap() = counter_before;
                } else {
                    res.unwrap_or_else(|e| panic!("unexpected create failure on {id}: {e:?}"));
                    if STATUS_POOL[status_idx] == "audited" {
                        record_step("hook_cascade");
                    }
                }
            }
            Transition::Patch { id_idx, status_idx, slug_idx, body_idx, extra } => {
                record_step("patch");
                let id = ID_POOL[id_idx];
                let mut fields = vec![
                    ("status".into(), CanonicalValue::String(STATUS_POOL[status_idx].into())),
                    ("body".into(), CanonicalValue::String(BODY_POOL[body_idx].into())),
                ];
                let slug = slug_idx.map(|i| SLUG_POOL[i]);
                if let Some(s) = slug {
                    fields.push(("slug".into(), CanonicalValue::String(s.into())));
                }
                if let ExtraFieldState::Active(fname) = pre.extra_state
                    && let Some(e) = extra
                {
                    fields.push((fname.into(), CanonicalValue::String(e)));
                }
                let res =
                    state.db.execute_batch(&[WireOp::Patch { target: EntityTarget::ExternalId(id.into()), fields }]);
                match pre.rows.get(id) {
                    None => {
                        let err = res.unwrap_err();
                        assert_eq!(err.code, ErrorCode::EntityNotFound, "patch missing {id}: {err:?}");
                        record_step("refusal_not_found");
                    }
                    Some(row) if row.trashed => {
                        let err = res.unwrap_err();
                        assert_eq!(err.code, ErrorCode::EntityTrashed, "patch trashed {id}: {err:?}");
                        record_step("refusal_trashed");
                    }
                    Some(_) => {
                        if slug.is_some_and(|s| pre.slug_owner(s).is_some_and(|o| o != id)) {
                            let err = res.unwrap_err();
                            assert_eq!(err.code, ErrorCode::UniqueViolation, "patch unique collision {id}: {err:?}");
                            record_step("refusal_unique_violation");
                        } else {
                            res.unwrap_or_else(|e| panic!("unexpected patch failure on {id}: {e:?}"));
                        }
                    }
                }
            }
            Transition::Unset { id_idx, clear_slug, clear_extra } => {
                record_step("unset");
                let id = ID_POOL[id_idx];
                let mut fields = Vec::new();
                if clear_slug {
                    fields.push("slug".to_owned());
                }
                if clear_extra && let ExtraFieldState::Active(fname) = pre.extra_state {
                    fields.push(fname.to_owned());
                }
                if !fields.is_empty() {
                    let res = state
                        .db
                        .execute_batch(&[WireOp::Unset { target: EntityTarget::ExternalId(id.into()), fields }]);
                    match pre.rows.get(id) {
                        None => assert_eq!(res.unwrap_err().code, ErrorCode::EntityNotFound),
                        Some(r) if r.trashed => assert_eq!(res.unwrap_err().code, ErrorCode::EntityTrashed),
                        Some(_) => {
                            res.unwrap();
                        }
                    }
                }
            }
            Transition::Trash { id_idx } => {
                record_step("trash");
                let id = ID_POOL[id_idx];
                let res = state.db.execute_batch(&[WireOp::Trash { target: EntityTarget::ExternalId(id.into()) }]);
                match pre.rows.get(id) {
                    None => assert_eq!(res.unwrap_err().code, ErrorCode::EntityNotFound),
                    Some(r) if r.trashed => assert_eq!(res.unwrap_err().code, ErrorCode::EntityTrashed),
                    Some(_) => {
                        res.unwrap();
                    }
                }
            }
            Transition::Restore { id_idx } => {
                record_step("restore");
                let id = ID_POOL[id_idx];
                let res = state.db.execute_batch(&[WireOp::Restore { target: EntityTarget::ExternalId(id.into()) }]);
                match pre.rows.get(id) {
                    None => assert_eq!(res.unwrap_err().code, ErrorCode::EntityNotFound),
                    Some(r) if !r.trashed => assert_eq!(res.unwrap_err().code, ErrorCode::EntityNotTrashed),
                    Some(_) => {
                        res.unwrap();
                    }
                }
            }
            Transition::Destroy { id_idx } => {
                record_step("destroy");
                let id = ID_POOL[id_idx];
                let res = state.db.execute_batch(&[WireOp::Destroy { target: EntityTarget::ExternalId(id.into()) }]);
                match pre.rows.get(id) {
                    None => assert_eq!(res.unwrap_err().code, ErrorCode::EntityNotFound),
                    Some(_) => {
                        res.unwrap();
                    }
                }
            }
            Transition::Link { src_idx, dst_idx } => {
                record_step("link");
                let src = ID_POOL[src_idx];
                let dst = ID_POOL[dst_idx];
                let res = state.db.execute_batch(&[WireOp::Link {
                    source: EntityTarget::ExternalId(src.into()),
                    rel_kind: "refs".into(),
                    target: EntityTarget::ExternalId(dst.into()),
                    weight: None,
                    info: None,
                }]);
                let src_row = pre.rows.get(src);
                let dst_row = pre.rows.get(dst);
                match (src_row, dst_row) {
                    (Some(s), Some(d)) if !s.trashed && !d.trashed => {
                        res.unwrap();
                    }
                    (None, _) | (_, None) => {
                        assert_eq!(res.unwrap_err().code, ErrorCode::EntityNotFound);
                    }
                    _ => {
                        assert_eq!(res.unwrap_err().code, ErrorCode::EntityTrashed);
                    }
                }
            }
            Transition::Unlink { src_idx, dst_idx } => {
                record_step("unlink");
                let src = ID_POOL[src_idx];
                let dst = ID_POOL[dst_idx];
                let res = state.db.execute_batch(&[WireOp::Unlink {
                    source: EntityTarget::ExternalId(src.into()),
                    rel_kind: Some("refs".into()),
                    target: Some(EntityTarget::ExternalId(dst.into())),
                }]);
                if !pre.rows.contains_key(src) || !pre.rows.contains_key(dst) {
                    let err = res.unwrap_err();
                    assert!(matches!(err.code, ErrorCode::EntityNotFound | ErrorCode::RelationNotFound));
                } else {
                    let _ = res;
                }
            }
            Transition::NestedSavepoint { keep_idx, abort_idx } => {
                record_step("nested_savepoint");
                let keep_id = ID_POOL[keep_idx];
                let abort_id = ID_POOL[abort_idx];
                if !pre.rows.contains_key(keep_id) {
                    let counter_before = *state.audit_counter.lock().unwrap();
                    state
                        .db
                        .transact_scoped(TransactOptions::default(), || {
                            state.db.execute_batch(&[WireOp::Create {
                                type_name: "Note".into(),
                                external_id: Some(keep_id.into()),
                                fields: vec![
                                    ("title".into(), CanonicalValue::String(TITLE_POOL[0].into())),
                                    ("status".into(), CanonicalValue::String("audited".into())),
                                    ("label".into(), CanonicalValue::String(LABEL_POOL[0].into())),
                                    ("body".into(), CanonicalValue::String(BODY_POOL[0].into())),
                                ],
                            }])?;
                            let inner_res: elmdb::Result<()> =
                                state.db.transact_scoped(TransactOptions::default(), || {
                                    if abort_id != keep_id && !pre.rows.contains_key(abort_id) {
                                        state.db.execute_batch(&[WireOp::Create {
                                            type_name: "Note".into(),
                                            external_id: Some(abort_id.into()),
                                            fields: vec![
                                                ("title".into(), CanonicalValue::String(TITLE_POOL[1].into())),
                                                ("status".into(), CanonicalValue::String("audited".into())),
                                                ("label".into(), CanonicalValue::String(LABEL_POOL[1].into())),
                                                ("body".into(), CanonicalValue::String(BODY_POOL[1].into())),
                                            ],
                                        }])?;
                                    }
                                    Err(Error::new(ErrorCode::ValidationFailed, "intentional inner savepoint abort"))
                                });
                            assert_eq!(inner_res.unwrap_err().code, ErrorCode::ValidationFailed);
                            Ok(())
                        })
                        .unwrap();
                    // Inner aborted before hooks ran; outer committed `keep_id` -> exactly +1 audit row!
                    assert_eq!(*state.audit_counter.lock().unwrap(), counter_before + 1);
                }
            }
            Transition::OuterRollback { abort_idx } => {
                record_step("outer_rollback");
                let abort_id = ID_POOL[abort_idx];
                let counter_before = *state.audit_counter.lock().unwrap();
                let res: elmdb::Result<()> = state.db.transact_scoped(TransactOptions::default(), || {
                    if !pre.rows.contains_key(abort_id) {
                        state.db.execute_batch(&[WireOp::Create {
                            type_name: "Note".into(),
                            external_id: Some(abort_id.into()),
                            fields: vec![("status".into(), CanonicalValue::String("audited".into()))],
                        }])?;
                    }
                    Err(Error::new(ErrorCode::ValidationFailed, "intentional outer rollback"))
                });
                assert_eq!(res.unwrap_err().code, ErrorCode::ValidationFailed);
                assert_eq!(*state.audit_counter.lock().unwrap(), counter_before);
            }
            Transition::RenameExtraField => {
                record_step("rename_field");
                if pre.extra_state == ExtraFieldState::Active("extra") {
                    state.db.rename_field("Note", "extra", "memo").unwrap();
                }
            }
            Transition::ArchiveAndPurgeExtra => {
                record_step("archive_and_purge");
                if let ExtraFieldState::Active(archived_name) = pre.extra_state {
                    state.db.set_schema(&oracle_schema(None, false)).unwrap();
                    if pre.scratch_active {
                        let purged = state.db.purge_archived("Scratch").unwrap();
                        assert!(purged >= 1, "purge_archived('Scratch') must purge seeded scratch row");
                        assert!(state.db.get_row("scratch-0").unwrap().is_none());
                    }
                    // Writing to an archived field must fail closed with `UnknownField`.
                    let err = state
                        .db
                        .execute_batch(&[WireOp::Create {
                            type_name: "Note".into(),
                            external_id: Some("archived-field-probe".into()),
                            fields: vec![(archived_name.into(), CanonicalValue::String("denied".into()))],
                        }])
                        .unwrap_err();
                    assert_eq!(err.code, ErrorCode::UnknownField, "writing archived field {archived_name}: {err:?}");
                }
            }
            Transition::DumpLoadVerify => {
                record_step("dump_load");
                let dump_dir = tempfile::tempdir().unwrap();
                let dump_file = dump_dir.path().join("oracle.elmdb");
                let loaded_dir = dump_dir.path().join("loaded");
                elmdb::dump::dump(&state.db, &dump_file).unwrap();
                elmdb::dump::load(&dump_file, &loaded_dir).unwrap();
                let loaded = Database::open(
                    &loaded_dir,
                    OpenOptions {
                        durability: DurabilityMode::Grouped,
                        build_policy: BuildPolicy::EagerInSchemaChange,
                        ..Default::default()
                    },
                )
                .unwrap();
                assert!(loaded.check(None).unwrap().clean);
                for id in ID_POOL {
                    let a = state.db.get_row_resolved(id).unwrap();
                    let b = loaded.get_row_resolved(id).unwrap();
                    assert_eq!(a, b, "dump/load resolved row mismatch on {id}");
                }
                loaded.close().unwrap();
            }
            Transition::CheckInvariants => {
                record_step("check_invariants");
            }
        }
        state.pre = post_ref.clone();
        state
    }

    fn check_invariants(state: &Self::SystemUnderTest, ref_state: &OracleRefState) {
        state.verify_rows_and_indexes(ref_state);
    }
}

prop_state_machine! {
    #![proptest_config(ProptestConfig {
        cases: 24,
        max_shrink_iters: 256,
        failure_persistence: None,
        .. ProptestConfig::default()
    })]

    #[test]
    fn database_deterministic_state_machine_oracle(sequential 1..28 => OracleSut);
}

#[test]
fn step_coverage_guard_runs_all_oracle_transitions() {
    // Run a targeted deterministic sweep using the same StateMachineTest harness so step coverage
    // is guaranteed even when cargo runs individual tests in isolation.
    let mut ref_state = OracleRefState::new();
    let mut sut = OracleSut::init_test(&ref_state);
    let script = vec![
        Transition::Create {
            id_idx: 0,
            title_idx: 1,
            status_idx: 2,
            slug_idx: Some(0),
            label_idx: 2,
            body_idx: 1,
            extra: Some("e1".into()),
        },
        Transition::Create {
            id_idx: 0,
            title_idx: 0,
            status_idx: 0,
            slug_idx: Some(1),
            label_idx: 0,
            body_idx: 0,
            extra: None,
        }, // AlreadyExists
        Transition::Create {
            id_idx: 1,
            title_idx: 0,
            status_idx: 0,
            slug_idx: Some(0),
            label_idx: 1,
            body_idx: 0,
            extra: None,
        }, // UniqueViolation
        Transition::Create {
            id_idx: 1,
            title_idx: 0,
            status_idx: 0,
            slug_idx: Some(1),
            label_idx: 1,
            body_idx: 0,
            extra: Some("e2".into()),
        },
        Transition::Patch { id_idx: 5, status_idx: 1, slug_idx: None, body_idx: 2, extra: None }, // NotFound
        Transition::Patch { id_idx: 1, status_idx: 1, slug_idx: Some(2), body_idx: 1, extra: Some("p1".into()) },
        Transition::Link { src_idx: 0, dst_idx: 1 },
        Transition::Unlink { src_idx: 0, dst_idx: 1 },
        Transition::Link { src_idx: 0, dst_idx: 1 },
        Transition::Trash { id_idx: 1 },
        Transition::Patch { id_idx: 1, status_idx: 0, slug_idx: None, body_idx: 0, extra: None }, // EntityTrashed
        Transition::Restore { id_idx: 1 },
        Transition::Unset { id_idx: 1, clear_slug: true, clear_extra: false },
        Transition::NestedSavepoint { keep_idx: 2, abort_idx: 3 },
        Transition::OuterRollback { abort_idx: 4 },
        Transition::RenameExtraField,
        Transition::DumpLoadVerify,
        Transition::ArchiveAndPurgeExtra,
        Transition::Destroy { id_idx: 0 },
        Transition::CheckInvariants,
    ];
    for t in script {
        ref_state = OracleRefMachine::apply(ref_state, &t);
        sut = OracleSut::apply(sut, &ref_state, t);
        OracleSut::check_invariants(&sut, &ref_state);
    }

    let covered = COVERED_STEPS.lock().unwrap();
    for required in [
        "create",
        "patch",
        "unset",
        "trash",
        "restore",
        "destroy",
        "link",
        "unlink",
        "nested_savepoint",
        "outer_rollback",
        "rename_field",
        "archive_and_purge",
        "dump_load",
        "check_invariants",
        "hook_cascade",
        "refusal_already_exists",
        "refusal_unique_violation",
        "refusal_not_found",
        "refusal_trashed",
    ] {
        assert!(covered.contains(required), "oracle step-coverage guard missed required step `{required}`");
    }
}

// ---------------------------------------------------------------------------
// Approximate-index oracle: Vector (HNSW SQ8), Rank (PPR), and Leiden
// ---------------------------------------------------------------------------

fn approx_schema() -> Schema {
    serde_json::from_value(serde_json::json!({
        "types": {
            "Doc": {
                "fields": {
                    "vec": { "type": "list", "of": { "type": "number" }, "optional": true }
                },
                "indexes": {
                    "by_vec": { "kind": "vector", "fields": ["vec"], "dims": 8, "quantization": "sq8" },
                    "by_rank": { "kind": "rank", "relations": ["cites"], "weighted": true }
                }
            }
        },
        "relations": {
            "cites": {
                "from": ["Doc"],
                "to": ["Doc"],
                "weighted": true
            }
        }
    }))
    .unwrap()
}

fn vec_to_canonical(v: &[f32]) -> CanonicalValue {
    CanonicalValue::Array(v.iter().map(|&f| CanonicalValue::float(f as f64).unwrap()).collect())
}

fn synth_unit_vec(seed: u64) -> Vec<f32> {
    let mut x = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) ^ 0xbf58_476d_1ce4_e5b9;
    let mut v = Vec::with_capacity(8);
    let mut norm_sq = 0.0f32;
    for _ in 0..8 {
        x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        let f = (((x >> 40) as i32) as f32 / (1u32 << 23) as f32) - 1.0;
        v.push(f);
        norm_sq += f * f;
    }
    let inv = norm_sq.sqrt().max(1e-6).recip();
    for c in &mut v {
        *c *= inv;
    }
    v
}

fn cosine(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b.iter()).map(|(x, y)| x * y).sum()
}

#[test]
fn approximate_indexes_match_rebuild_and_recall_bounds() {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(
        dir.path(),
        OpenOptions {
            durability: DurabilityMode::Grouped,
            build_policy: BuildPolicy::EagerInSchemaChange,
            map_size: Some(16 * 1024 * 1024),
            ..Default::default()
        },
    )
    .unwrap();
    db.set_schema(&approx_schema()).unwrap();

    // Populate 32 Doc entities with deterministic unit vectors and 2 cluster communities of edges,
    // then mutate (update embeddings + destroy + re-link) so incremental HNSW/Rank maintenance is exercised.
    let mut live_vecs: BTreeMap<String, Vec<f32>> = BTreeMap::new();
    let mut create_ops = Vec::new();
    for i in 0..32u64 {
        let id = format!("d{i:02}");
        let v = synth_unit_vec(100 + i);
        live_vecs.insert(id.clone(), v.clone());
        create_ops.push(WireOp::Create {
            type_name: "Doc".into(),
            external_id: Some(id),
            fields: vec![("vec".into(), vec_to_canonical(&v))],
        });
    }
    db.execute_batch(&create_ops).unwrap();
    for (id, v) in &live_vecs {
        db.set_embeddings("by_vec", id, std::slice::from_ref(v)).unwrap();
    }

    // Create two strongly-connected clusters (`d00..d15` and `d16..d31`) with sparse bridges.
    let mut link_ops = Vec::new();
    for i in 0..32usize {
        let base = if i < 16 { 0 } else { 16 };
        let next1 = base + ((i - base + 1) % 16);
        let next2 = base + ((i - base + 2) % 16);
        for dst in [next1, next2] {
            link_ops.push(WireOp::Link {
                source: EntityTarget::ExternalId(format!("d{i:02}")),
                rel_kind: "cites".into(),
                target: EntityTarget::ExternalId(format!("d{dst:02}")),
                weight: Some(1.0f64.to_bits()),
                info: None,
            });
        }
    }
    link_ops.push(WireOp::Link {
        source: EntityTarget::ExternalId("d00".into()),
        rel_kind: "cites".into(),
        target: EntityTarget::ExternalId("d16".into()),
        weight: Some(0.25f64.to_bits()),
        info: None,
    });
    db.execute_batch(&link_ops).unwrap();

    // Mutate several vectors and destroy 4 entities to exercise incremental updates + tombstones.
    let mut mut_ops = Vec::new();
    for i in 0..4u64 {
        let id = format!("d{i:02}");
        let v = synth_unit_vec(900 + i);
        live_vecs.insert(id.clone(), v.clone());
        db.set_embeddings("by_vec", &id, std::slice::from_ref(&v)).unwrap();
        mut_ops.push(WireOp::Patch {
            target: EntityTarget::ExternalId(id),
            fields: vec![("vec".into(), vec_to_canonical(&v))],
        });
    }
    for i in [7usize, 15, 23, 31] {
        let id = format!("d{i:02}");
        live_vecs.remove(&id);
        mut_ops.push(WireOp::Destroy { target: EntityTarget::ExternalId(id) });
    }
    db.execute_batch(&mut_ops).unwrap();

    // Drive background HNSW / Rank catch-up to completion.
    for _ in 0..64 {
        if !db.drive_background_work(64).unwrap() {
            break;
        }
    }
    assert!(db.check(None).unwrap().clean);

    // Helper: measure average HNSW recall@5 against brute-force cosine across 8 queries.
    let measure_recall = |target_db: &Database| -> f64 {
        let mut hits_total = 0usize;
        let queries = 8u64;
        let k = 5usize;
        for q in 0..queries {
            let qv = synth_unit_vec(5000 + q);
            let mut brute: Vec<(String, f32)> = live_vecs.iter().map(|(id, v)| (id.clone(), cosine(&qv, v))).collect();
            brute.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
            let truth: BTreeSet<String> = brute.into_iter().take(k).map(|(id, _)| id).collect();

            let spec = QuerySpec {
                source: QuerySource::Type { type_name: "Doc".into(), include_trashed: false },
                steps: vec![QueryStep::Score {
                    spec: QueryScore::VectorNear { index: "by_vec".into(), vector: qv, top_k: k },
                }],
                terminal: Some(QueryTerminal::Ids),
                explain: false,
            };
            let got = ids_from_answer(&target_db.execute_query_spec(&spec, ExecBudgets::default()).unwrap());
            for id in &got {
                assert!(live_vecs.contains_key(id), "HNSW returned deleted/ghost entity {id}");
            }
            hits_total += got.intersection(&truth).count();
        }
        hits_total as f64 / (queries as usize * k) as f64
    };

    let recall_before = measure_recall(&db);
    assert!(recall_before >= 0.85, "incremental HNSW recall@5 too low: {recall_before:.3}");

    // Snapshot RankIndex, GraphPpr, and GraphLeiden before rebuild.
    let rank_spec = QuerySpec {
        source: QuerySource::Type { type_name: "Doc".into(), include_trashed: false },
        steps: vec![QueryStep::Score { spec: QueryScore::RankIndex { index: "by_rank".into() } }],
        terminal: Some(QueryTerminal::Hits { fields: None, highlights: false }),
        explain: false,
    };
    let ppr_spec = QuerySpec {
        source: QuerySource::Type { type_name: "Doc".into(), include_trashed: false },
        steps: vec![QueryStep::GraphPpr {
            rel_kinds: vec!["cites".into()],
            weighted: true,
            seeds: vec![("d00".into(), 1.0)],
            damping: 0.85,
            iterations: 40,
            tolerance: 1e-7,
        }],
        terminal: None,
        explain: false,
    };
    let leiden_spec = QuerySpec {
        source: QuerySource::Type { type_name: "Doc".into(), include_trashed: false },
        steps: vec![QueryStep::GraphLeiden {
            rel_kinds: vec!["cites".into()],
            weighted: true,
            resolution: 1.0,
            seed: 42,
            iterations: 10,
        }],
        terminal: None,
        explain: false,
    };

    let rank_before = hits_map(&db.execute_query_spec(&rank_spec, ExecBudgets::default()).unwrap());
    let ppr_before = ppr_map(&db.execute_query_spec(&ppr_spec, ExecBudgets::default()).unwrap());
    let leiden_before = db.execute_query_spec(&leiden_spec, ExecBudgets::default()).unwrap();

    // Full rebuild from primary (`derived == rebuild(primary)`).
    db.rebuild(None).unwrap();
    for _ in 0..64 {
        if !db.drive_background_work(64).unwrap() {
            break;
        }
    }
    assert!(db.check(None).unwrap().clean);

    let recall_after = measure_recall(&db);
    assert!(recall_after >= 0.85, "rebuilt HNSW recall@5 too low: {recall_after:.3}");

    let rank_after = hits_map(&db.execute_query_spec(&rank_spec, ExecBudgets::default()).unwrap());
    let ppr_after = ppr_map(&db.execute_query_spec(&ppr_spec, ExecBudgets::default()).unwrap());
    let leiden_after = db.execute_query_spec(&leiden_spec, ExecBudgets::default()).unwrap();

    assert_eq!(rank_before.len(), live_vecs.len());
    assert_eq!(rank_before.len(), rank_after.len());
    for (id, s_before) in &rank_before {
        let s_after = rank_after[id];
        assert!(
            (s_before - s_after).abs() <= 1e-4,
            "RankIndex score diverged after rebuild on {id}: before={s_before}, after={s_after}"
        );
    }
    assert_eq!(ppr_before.len(), ppr_after.len());
    for (id, s_before) in &ppr_before {
        let s_after = ppr_after[id];
        assert!(
            (s_before - s_after).abs() <= 1e-6,
            "GraphPpr diverged after rebuild on {id}: before={s_before}, after={s_after}"
        );
    }
    assert_eq!(leiden_before, leiden_after, "GraphLeiden (fixed seed) must match identically after rebuild");

    db.close().unwrap();
}

fn hits_map(ans: &elmdb::protocol::QueryAnswer) -> BTreeMap<String, f64> {
    match &ans.body {
        QueryAnswerBody::Hits { hits } => hits.iter().map(|h| (h.id.clone(), h.score)).collect(),
        other => panic!("expected Hits, got {other:?}"),
    }
}

fn ppr_map(ans: &elmdb::protocol::QueryAnswer) -> BTreeMap<String, f64> {
    match &ans.body {
        QueryAnswerBody::GraphPpr { scores, .. } => scores.iter().map(|h| (h.id.clone(), h.score)).collect(),
        other => panic!("expected GraphPpr, got {other:?}"),
    }
}
