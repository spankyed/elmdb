# The JavaScript boundary

Status: idea.

## Where it starts

Every call crosses N-API, opens a read transaction (heed can't renew one) and moves msgpack both ways: about 3 µs before
any work, and a commit's crossing about 45 µs (`../evidence/phase-10.md`, D17's re-reading). A query answering many
entities pays that once; a loop of one-entity reads pays it each time, and decoding is most of a long read's cost
(`../evidence/phase-1.md`). Writes have the opposite constraint: LMDB has one write transaction, so a unit held open
across an `await` holds every other writer off (`UNIT_OPEN`), and the background index builder waits on it.

## The idea

Two halves, the read side and the write side of one boundary.

- **Reads without a decode.** A row reaches JavaScript as a view over bytes the engine shares — an object whose fields
  decode when first read — so reading three fields of a row decodes three values, and a list of rows costs what is
  looked at. Read transactions are kept per thread and renewed rather than opened, and a builder's operations queue and
  cross once per unit.
- **Units that can span an `await`.** A unit reads from a snapshot and buffers its writes; its commit takes the write
  transaction briefly, checks that nothing it read has changed since its snapshot, and applies the writes or refuses
  with a conflict the caller retries. No unit holds the writer while JavaScript waits, and the builder never waits on
  one.

## Open questions

- Shared memory's lifetime against LMDB's: a view outliving its read transaction must not read freed pages, so the
  bytes are copied out or the transaction pinned, and either has a cost to measure.
- What a unit's read set is for validation: the tracked reads' key ranges are one answer.
- Conflict rates under apack's real workload, which decide whether optimistic units are a win.
