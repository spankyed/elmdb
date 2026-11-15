//! Where a write transaction is committed: this module and no other, so what must hold of every commit is held in
//! one place (`docs/roadmap/synced-commit.md`). A commit made anywhere else in the engine, a new environment's first
//! or a checkpoint's as much as a unit's, would be one that place never hears of, and `cargo xtask check-commits`
//! fails on it.

use heed::{CommitOrAbort, DatabaseOpener, Env, OnCommit, RwTxn, WithoutTls};

/// Commits a write transaction
pub(super) fn commit(txn: RwTxn<'_>) -> heed::Result<()> {
    txn.commit()
}

/// Makes databases in a write transaction and commits it, as `Env::create_databases_and_commit` does, which commits
/// the transaction itself: the databases are the environment's once the commit has landed
pub(super) fn create_databases<'e, 'a: 'e, D, A, E>(
    env: &'a Env<WithoutTls>,
    wtxn: RwTxn<'e>,
    create: impl FnOnce(&DatabaseOpener<'e, WithoutTls>, &mut RwTxn<'e>) -> Result<CommitOrAbort<D, A>, E>,
) -> Result<CommitOrAbort<D::Committed, A>, E>
where
    D: OnCommit,
    E: From<heed::Error>,
{
    env.create_databases_and_commit(wtxn, create)
}
