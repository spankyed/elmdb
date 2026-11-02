//! A rank index's scores held to the bound it reports of `rank` over the same graph: the comparison the rank index's
//! own specs (`rank_index.rs`) and the model harness (`model.rs`) both make

use serde_json::{Value as Json, json};

use crate::Database;
use crate::engine::plan::{ReadOp, Reads};

/// A rank index as a read names it, and what `rank` computes it against
pub(super) struct RankIndex<'a> {
    pub name: &'a str,
    pub types: &'a [&'a str],
    pub kinds: &'a [&'a str],
    pub direction: &'a str,
    pub weights: bool,
}

/// One plan's answer, as JSON
pub(super) fn read(db: &Database, plan: Json) -> crate::Result<Json> {
    let reads = Reads {
        reads: vec![serde_json::from_value::<ReadOp>(json!({ "read": "query", "plan": plan })).unwrap()],
        track: None,
    };
    db.read(&reads)
        .map(|bytes| serde_json::to_value(rmpv::decode::read_value(&mut &bytes[..]).unwrap()).unwrap()[0].clone())
}

/// `id → score` from a `hits` answer, sorted by id, and the answer's `rank` record
fn scores(answer: &Json) -> (Vec<(String, f64)>, Json) {
    let mut hits: Vec<_> = answer["hits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| (h["id"].as_str().unwrap().to_owned(), h["score"].as_f64().unwrap()))
        .collect();
    hits.sort_by(|a, b| a.0.cmp(&b.0));
    (hits, answer["rank"].clone())
}

impl RankIndex<'_> {
    /// The index's scores and bound, after its pushes are done
    pub(super) fn indexed(&self, db: &Database) -> (Vec<(String, f64)>, f64) {
        db.wait_for_index_builds().unwrap();
        let answer = read(
            db,
            json!({ "start": { "from": "types", "types": self.types },
            "steps": [{ "step": "ranked", "index": self.name }], "terminal": { "answer": "hits" } }),
        )
        .unwrap();
        let (found, rank) = scores(&answer);
        (found, rank["error"].as_f64().unwrap())
    }

    /// `rank` over the same graph, every entity a seed, run far past the index's tolerance
    pub(super) fn computed(&self, db: &Database) -> Vec<(String, f64)> {
        let answer = read(
            db,
            json!({ "start": { "from": "types", "types": self.types },
            "steps": [{ "step": "rank", "kinds": self.kinds, "direction": self.direction, "weights": self.weights,
                       "iterations": 100000, "tolerance": 1e-14 }],
            "terminal": { "answer": "hits" } }),
        )
        .unwrap();
        scores(&answer).0
    }

    /// The index's scores against `rank`'s: the same entities, within the bound the index reports, and in `rank`'s
    /// order wherever two differ by more than twice it. Answers the entities, sorted
    pub(super) fn assert_within_bound(&self, db: &Database) -> Vec<String> {
        let (found, error) = self.indexed(db);
        let truth = self.computed(db);
        let ids: Vec<String> = found.iter().map(|f| f.0.clone()).collect();
        assert_eq!(ids, truth.iter().map(|t| t.0.clone()).collect::<Vec<_>>());
        let distance: f64 = found.iter().zip(&truth).map(|(f, t)| (f.1 - t.1).abs()).sum();
        assert!(distance <= error + 1e-12, "{distance} past the bound {error}: {found:?} against {truth:?}");
        for (a, ta) in found.iter().zip(&truth) {
            for (b, tb) in found.iter().zip(&truth) {
                if ta.1 - tb.1 > 2.0 * error + 1e-12 {
                    assert!(a.1 > b.1, "{} above {} by rank, not by the index", a.0, b.0);
                }
            }
        }
        ids
    }
}
