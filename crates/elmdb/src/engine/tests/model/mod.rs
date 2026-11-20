//! The model harness (`docs/architecture/testing.md`), ported from apack's `tests/_harness/`: random writes over a small pool of ids,
//! so they collide, refer to each other and are refused, checked after every operation against a reference model kept
//! as plainly as a model can be. The engine must hold what the model holds, refuse what it refuses, and keep every
//! derived key in step: a type's live and trashed entities in creation order, its count, each value index's entries,
//! the role index, each entity's edges out and in, and what a text index matches, whether maintained by writes or
//! derived again by a rebuild; and a rank index over the entities and their edges holding the model's live entities,
//! within the bound it reports of `rank` once its drain is done.

use std::collections::{BTreeMap, BTreeSet};

use proptest::prelude::*;
use serde_json::{Value as Json, json};

use super::rank_bound::RankIndex;
use super::{edges as stored_edges, row, schema_json};
use crate::ChangeSet;
use crate::engine::unit::Read;
use crate::engine::{entities, keys, relations, values};
use crate::format::key::{Part, decode as decode_key};
use crate::schema::Schema;
use crate::{Database, OpenOptions};

mod cases;
mod changes;
mod crash;
mod drive;
mod generate;
mod machine;
mod reads;
mod reference;
mod setup;

use changes::*;
use drive::*;
use generate::*;
use machine::*;
use reads::*;
use reference::*;
use setup::*;

const IDS: [&str; 6] = ["Task-1", "Task-2", "Task-3", "Task-4", "Note-1", "Note-2"];
const KEYS: [&str; 3] = ["a", "b", "c"];
const KINDS: [&str; 2] = ["refs", "blocks"];
/// The keyed kind: `cites`, keyed by its info's `n`, which the model's edges of it carry from a pool of two
const KEYED: &str = "cites";
/// Every kind the derived reads compare
const ALL_KINDS: [&str; 3] = ["refs", "blocks", KEYED];
const ROLES: [&str; 2] = ["r1", "r2"];
/// Every index rebuilt, or one of each kind
const REBUILDS: [Option<(&str, &str)>; 4] =
    [None, Some(("Task", "a")), Some(("Note", "text")), Some(("Note", "grams"))];
/// The engine's own fields, which the model leaves out of a row
const ENGINE_FIELDS: [&str; 5] = ["createdAt", "updatedAt", "shortCode", "trashedAt", "role"];

/// One write the workload makes, as data: the model and the engine each apply it
#[derive(Debug, Clone)]
enum Step {
    Create {
        id: &'static str,
        fields: BTreeMap<&'static str, Json>,
    },
    Set {
        id: &'static str,
        key: &'static str,
        value: Json,
    },
    Append {
        id: &'static str,
        key: &'static str,
        value: i64,
    },
    Delete {
        id: &'static str,
        key: &'static str,
    },
    Link {
        id: &'static str,
        kind: &'static str,
        target: &'static str,
    },
    Unlink {
        id: &'static str,
        kind: &'static str,
        target: &'static str,
    },
    /// Links by `cites` with the key `n`: the edge holding it, or a new one beside the pair's others
    LinkKeyed {
        id: &'static str,
        target: &'static str,
        n: u8,
    },
    /// Changes the key of the `cites` edge holding `from` to `to`, by its id; refused when there is none, or when
    /// another edge of the pair holds `to`
    Rekey {
        id: &'static str,
        target: &'static str,
        from: u8,
        to: u8,
    },
    Destroy {
        id: &'static str,
    },
    Trash {
        id: &'static str,
    },
    Restore {
        id: &'static str,
    },
    Grant {
        id: &'static str,
        role: &'static str,
    },
    Revoke {
        id: &'static str,
        role: &'static str,
    },
    Upsert {
        id: &'static str,
        fields: BTreeMap<&'static str, Json>,
    },
    Merge {
        id: &'static str,
        key: &'static str,
        value: Json,
    },
    RemoveAt {
        id: &'static str,
        key: &'static str,
        index: u32,
    },
    RemoveValue {
        id: &'static str,
        key: &'static str,
        value: Json,
    },
    /// Grants the role, revoking it from every other id that holds it
    GrantExclusive {
        id: &'static str,
        role: &'static str,
    },
    /// Removes the first relation between the ends, by its id; refused when there is none
    RemoveRelation {
        id: &'static str,
        kind: &'static str,
        target: &'static str,
    },
    /// Moves the first relation between the ends to another target, by its id; refused when there is none, or when the
    /// new ends are joined already
    MoveRelation {
        id: &'static str,
        kind: &'static str,
        target: &'static str,
        to: &'static str,
    },
    TakeLabel {
        id: &'static str,
    },
    /// Writes made as one unit, which then commits or throws; inside another unit, a savepoint whose refusal rolls back
    /// its own writes while the outer one goes on
    Unit {
        steps: Vec<Step>,
        throws: bool,
        durable: bool,
    },
    /// The store refuses the next commit it is asked to make, as a full disk would
    RefuseNext,
    /// Derived data deleted and derived again from primary data, every index's or one's: the reconstruction `check()`
    /// shares, so only the model's answers can tell it wrong
    Rebuild {
        index: Option<(&'static str, &'static str)>,
    },
    /// Every entity, edge and derived row removed; the schema, the meta data and the sequence counter are kept
    Reset,
    /// Every edge of one kind moved to another: a commit with no change set, which stamps the ends of each edge it moved
    RenameRelation {
        from: &'static str,
        to: &'static str,
    },
    /// A `Task` field's values moved to another: the harness declares `spare` as a second unindexed field of `Task`
    RenameField {
        from: &'static str,
        to: &'static str,
    },
    /// A copy of the database as it is, into `model-backup` inside its directory, the copy before replaced
    Backup,
    /// A close that gives up, the store held past its wait: it stops the builder, commits nothing, and leaves the
    /// database open with what was left to build queued again for another thread
    CloseGivesUp,
    /// Every row replaced with those of the last backup: refused with no backup, and otherwise a commit with no change
    /// set, after which a reader from before it is told the answer is incomplete, as after a reset
    ReplaceWith,
    /// The schema set with the spare names if it lacks them, and without them if it has them (`harness_schema`): a commit
    /// that stamps nothing. Data under a removed name stays until purged
    Redeclare,
    /// The spare field's values purged from every Task: refused while the schema declares it
    PurgeField,
    /// The `tags` edges purged: refused while the schema declares the kind
    PurgeKind,
    /// `Note` renamed to `Memo`, a type the harness declares with `Note`'s fields and indexes, and back, in one commit:
    /// what the two renames leave is what was there, and a reader of `Note` from before it is told the answer is
    /// incomplete, as after a reset. One commit, so that a crash leaves the ids as they were or as they are again
    RenameTypeAndBack,
}

/// The field renames the workload makes, both ways between `c` and the spare field
const FIELD_RENAMES: [(&str, &str); 2] = [("c", "spare"), ("spare", "c")];

/// The kinds a relation rename moves between: the workload's two plain kinds and the one it never links
const RENAMES: [(&str, &str); 6] = [
    ("refs", "tags"),
    ("tags", "refs"),
    ("blocks", "tags"),
    ("tags", "blocks"),
    ("refs", "blocks"),
    ("blocks", "refs"),
];
