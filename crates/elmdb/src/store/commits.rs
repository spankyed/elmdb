//! Where a write transaction is committed: this module and no other, so what must hold of every commit is held in
//! one place (`docs/roadmap/synced-commit.md`). A commit made anywhere else in the engine, a new environment's first
//! or a checkpoint's as much as a unit's, would be one that place never hears of, and `cargo xtask check-commits`
//! fails on it.
//!
//! What holds here, in the grouped mode: a commit is made only while the process holds the pin, which its write made
//! sure of before its transaction began (`kept.rs`), and the commit is the process's last until a later one, which a
//! sync reads to tell whether the process owes it a record.

use heed::{CommitOrAbort, DatabaseOpener, Env, OnCommit, RwTxn, WithoutTls};

use super::kept::Kept;
use crate::error::{Error, ErrorCode};

/// Commits a write transaction
pub(super) fn commit(txn: RwTxn<'_>, kept: Option<&Kept>) -> Result<(), Error> {
    let id = pinned(&txn, kept)?;
    txn.commit()?;
    if let Some(kept) = kept {
        kept.committed(id);
    }
    Ok(())
}

/// Makes databases in a write transaction and commits it, as `Env::create_databases_and_commit` does, which commits
/// the transaction itself: the databases are the environment's once the commit has landed
pub(super) fn create_databases<'e, 'a: 'e, D, A, E>(
    env: &'a Env<WithoutTls>,
    wtxn: RwTxn<'e>,
    kept: Option<&Kept>,
    create: impl FnOnce(&DatabaseOpener<'e, WithoutTls>, &mut RwTxn<'e>) -> Result<CommitOrAbort<D, A>, E>,
) -> Result<CommitOrAbort<D::Committed, A>, E>
where
    D: OnCommit,
    E: From<heed::Error> + From<Error>,
{
    let id = pinned(&wtxn, kept)?;
    let made = env.create_databases_and_commit(wtxn, create)?;
    if let (Some(kept), CommitOrAbort::Commit(_)) = (kept, &made) {
        kept.committed(id);
    }
    Ok(made)
}

/// The transaction's id, once it is known to be committed under the pin where the grouped mode keeps one
fn pinned(txn: &RwTxn<'_>, kept: Option<&Kept>) -> Result<u64, Error> {
    // A sync that failed since the write made sure of its pin: the commit would be built on pages that may be gone
    if let Some(refused) = kept.and_then(Kept::refused) {
        return Err(refused);
    }
    if kept.is_some_and(|kept| !kept.pinned()) {
        return Err(Error::new(
            ErrorCode::Io,
            "A write reached its commit without the pin that keeps the last synced commit's pages, and was not \
             committed: it is a defect of elmdb's, to be reported",
        ));
    }
    Ok(txn.id() as u64)
}
