# The JavaScript boundary

Status: partly built; the rest are ideas, each waiting on a measurement or a feature.

## Where it starts

Every call crosses N-API and moves msgpack both ways. A single row's read costs about 2.8 µs, its read transaction taken
from the store's idle ones while still the last commit's snapshot rather than opened (`a0da984`: from about 3.69 µs,
2026-10-25, a busy machine, both builds alternated ten rounds). LMDB's own idiom is a transaction reset and renewed
(`mdb_txn_reset`, `mdb_txn_renew`); heed exposes neither, and keeping the transaction while nothing has committed skips
even a renew's work, so the reuse needs no patched heed. A write outside a unit is a commit of its own, about 38–60 µs;
inside one, about 11 µs (`../evidence/phase-10.md`, D17's re-reading). A query answering many entities pays the crossing
once; a loop of one-entity calls pays it each time, and decoding is most of a long read's cost
(`../evidence/phase-1.md`).

**What one action costs is calls per action, not the cost per call.** A cold boot of apack with its shipped pack, the
heaviest write path (every content item applied), made 900 reads, 174 commits outside a unit and one unit of 470 writes:
about 26 ms in elmdb out of 2.3 s, the unit-less commits about 12 ms (2026-10-25, measured by wrapping the binding's
calls in an apack runtime spec). A chat turn and a flow run are not measured yet, and are the next reading to take.

## Settled

- **Reads can't be deferred.** apack's reads are synchronous and return values, so nothing can queue them for later.
  One-entity reads in a loop are fixed in apack, as one set read (`query.ids(list)`, `pickAll`), where most of the gap
  to lmdb-js's in-memory reads is.
- **Writes aren't batched behind the caller's back.** A write throws its refusal at the call that made it, and code
  relies on that — a nested `transact` used as a savepoint among it. Queuing writes until a later read or the commit
  moves the throw away from the cause; JavaScript gives no signal when a builder chain ends to flush at. Grouping is
  explicit: `transact` for one commit, `setMany` for one operation, used where a reading shows the commits matter
  (at boot they don't: 12 ms of 2.3 s).

## Ideas

- **Reads without a decode.** A row reaches JavaScript as a view over bytes the engine shares, its fields decoded when
  first read. The riskiest item here: LMDB's memory is valid only while its read transaction is open, so a view that
  outlives the transaction reads freed or rewritten pages. It is safe only by copying the bytes out or by an epoch that
  keeps the transaction while any view of it lives, and a proxy's property access often costs more than the decode it
  saves. Measured on a hot path before anything is built.
- **Units that can span an `await`.** A unit reads from a snapshot and buffers its writes; its commit takes the write
  transaction briefly, checks that nothing it read has changed since — the key ranges tracked reads already record
  (`../architecture/engine.md`, Change sets and watch) — and applies the writes or refuses with a conflict the caller
  retries. FoundationDB's model, of which elmdb now has the ingredient. It changes what `transact` means for apack, so
  it waits for a flow that needs to hold a unit across an `await`.

## Open questions

- The crossings of a chat turn and a flow run, and which of their calls are loops.
- Conflict rates under apack's real workload, which decide whether optimistic units are a win.
