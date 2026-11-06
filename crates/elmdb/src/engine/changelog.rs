//! The change log: what each commit changed, written in the commit's own transaction, so that another process can learn
//! what a commit it didn't make changed. A change set otherwise goes only to whoever made the commit.
//!
//! A reader holds a cursor, the last sequence it has read, and asks for what came after it (`changes_since`). The log is
//! bounded: a reader further behind than it reaches is told so (`truncated`), and starts again from the data, as a boot
//! does. The log is neither primary data nor derived from it, so dumps leave it out and `rebuild()` leaves it alone.

use std::collections::BTreeSet;
use std::ops::Bound;

use serde::{Deserialize, Serialize};

use super::changes::ChangeSet;
use super::keys;
use crate::error::Result;
use crate::format::meta;
use crate::store::{Db, meta_key};

/// How many commits the log keeps. A reader this far behind has more catching up to do than a resync would cost
pub const LOG_COMMITS: u64 = 4096;

/// How often a commit also drops what has fallen out of the log's reach, which the log may run over by this many until then
pub const LOG_TRIM_EVERY: u64 = 64;

/// How many entities a record names before it says only their types, so one large commit can't make a large record
pub const LOG_ENTITIES: usize = 1024;

/// What one commit changed, as the log holds it
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "codegen", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct LoggedCommit {
    /// The commit's sequence
    #[cfg_attr(feature = "codegen", ts(type = "number"))]
    pub sequence: u64,
    /// The handle that made it, which `Database::origin` names, so a reader can tell its own from another's
    pub origin: String,
    /// Its writes aren't known: an emptied or replaced database, a schema change, a load, an admin unit
    pub everything: bool,
    /// The types whose entities changed
    pub types: BTreeSet<String>,
    /// The relation kinds whose edges changed
    pub kinds: BTreeSet<String>,
    /// The entities it changed, for a reader to read as they are now, which a record of what happened to each couldn't
    /// improve on and could only contradict; absent when there were more than `LOG_ENTITIES`
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "codegen", ts(optional))]
    pub entities: Option<Vec<String>>,
}

impl LoggedCommit {
    /// A commit whose writes aren't known, which any commit is until it says otherwise
    pub(crate) fn everything() -> Self {
        Self { everything: true, ..Self::default() }
    }

    /// A commit that wrote only derived data, a builder's: no row changed, whatever a reader of an index may now see
    pub(crate) fn derived() -> Self {
        // mutant-skip: replace LoggedCommit::derived -> Self with Default::default()
        // This is what the name says of it: a record that names nothing and doesn't say everything
        Self::default()
    }

    pub(crate) fn of(changes: &ChangeSet) -> Self {
        Self {
            types: changes.types.clone(),
            kinds: changes.kinds.clone(),
            entities: (changes.entities.len() <= LOG_ENTITIES).then(|| changes.entities.keys().cloned().collect()),
            ..Self::default()
        }
    }
}

/// What came after a cursor
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "codegen", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct Changes {
    /// The cursor to ask from next: the last commit these cover
    #[cfg_attr(feature = "codegen", ts(type = "number"))]
    pub through: u64,
    /// The log no longer reaches the cursor, so `commits` doesn't say everything since it: start again from the data
    pub truncated: bool,
    pub commits: Vec<LoggedCommit>,
    /// A unit's own changes so far, when a unit asked
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "codegen", ts(optional))]
    pub pending: Option<ChangeSet>,
}

/// Files `commit` under its sequence, and drops what has fallen out of the log's reach
pub(crate) fn file(txn: &mut heed::RwTxn<'_>, db: Db, commit: &LoggedCommit) -> Result<()> {
    let bytes = rmp_serde::to_vec_named(commit).expect("a logged commit serializes");
    db.main.put(txn, &keys::change_log(commit.sequence), &bytes)?;
    // A delete copies the pages down to the oldest record, as the put did down to the newest, so deleting one per commit
    // would add a second path to every commit: trimming in batches pays that once in a while
    if !commit.sequence.is_multiple_of(LOG_TRIM_EVERY) {
        return Ok(());
    }
    let Some(oldest) = commit.sequence.checked_sub(LOG_COMMITS) else { return Ok(()) };
    let prefix = keys::change_log_prefix();
    let reach = keys::change_log(oldest);
    let stale: Vec<Vec<u8>> = db
        .main
        .range(txn, &(Bound::Included(prefix.as_slice()), Bound::Included(reach.as_slice())))?
        .map(|item| item.map(|(key, _)| key.to_vec()))
        .collect::<std::result::Result<_, _>>()?;
    for key in stale {
        db.main.delete(txn, &key)?;
    }
    super::changed::trim_tombstones(txn, db, oldest)
}

/// What the log holds after `cursor`, up to `limit` commits, as of `txn`
pub(crate) fn since(txn: &heed::RoTxn<'_, heed::AnyTls>, db: Db, cursor: u64, limit: usize) -> Result<Changes> {
    let last = db
        .get(txn, &meta_key(meta::SEQUENCE))?
        .and_then(|bytes| <[u8; 8]>::try_from(bytes).ok())
        .map_or(0, u64::from_be_bytes);
    if limit == 0 {
        return Err(crate::error::Error::new(
            crate::error::ErrorCode::BatchInvalid,
            "A read of the change log asks for at least one commit",
        ));
    }
    if cursor == last {
        return Ok(Changes { through: last, ..Changes::default() });
    }
    // A cursor past every commit this database has made isn't one of its own: the log can't say what it missed
    let Some(next) = cursor.checked_add(1).filter(|_| cursor < last) else {
        return Ok(Changes { through: last, truncated: true, ..Changes::default() });
    };
    let prefix = keys::change_log_prefix();
    let start = keys::change_log(next);
    let mut commits = Vec::new();
    for item in db.main.range(txn, &(Bound::Included(start.as_slice()), Bound::Unbounded))? {
        let (key, value) = item?;
        if !key.starts_with(&prefix) || commits.len() == limit {
            break;
        }
        let commit: LoggedCommit = rmp_serde::from_slice(value).map_err(|error| {
            crate::error::Error::new(crate::error::ErrorCode::Corrupted, format!("A change log record: {error}"))
        })?;
        commits.push(commit);
    }
    let truncated = commits.first().is_none_or(|first| first.sequence != next);
    let through = if truncated { last } else { commits.last().map_or(last, |commit| commit.sequence) };
    Ok(Changes { through, truncated, commits, pending: None })
}
