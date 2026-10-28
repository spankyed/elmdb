# The JavaScript boundary

Status: designed, 2026-10-28. Set reads and explicit grouping are built and settled; reads without a decode and units
that span an `await` are designed here, each with the reading that would trigger it (`../evidence/next-phase-10.md`,
The JavaScript boundary).

## Where it stands

Every call crosses N-API and moves msgpack both ways. A single row's read costs about 2.8 µs, its read transaction
taken from the store's idle ones while still the last commit's snapshot (`a0da984`). A write outside a unit is a commit
of its own, about 38–60 µs; inside one, about 11 µs (`../evidence/phase-10.md`, D17's re-reading).

**What one action costs is calls per action, not the cost per call**, and Phase 3 counted them
(`../evidence/next-phase-3.md`): a chat turn with a 400-word reply makes 858 reads and 222 writes outside a unit, about
210 ms in elmdb of its 24 s; a flow run 84 reads and 12 writes, about 15 ms; a cold boot 900 reads, 174 commits and one
unit of 470 writes, about 26 ms of 2.3 s. No action wrote in a unit. The largest loop, a streamed chunk's existence
read before its write, is 15 ms of the turn's 24 s.

## Settled

- **Reads can't be deferred.** apack's reads are synchronous and return values. One-entity reads in a loop are fixed in
  apack, as one set read (`query.ids(list)`, `pickAll`).
- **Writes aren't batched behind the caller's back.** A write throws its refusal at the call that made it. Grouping is
  explicit: `transact` for one commit, `setMany` for one operation, where a reading shows the commits matter.

## The measurements

- **Decoding is half of a long read.** Every Message's whole row on G8's data (19,681 rows, a 148 MB answer, most of it
  tool-activity blocks): 266 ms for the engine to read, decompress, encode and cross; 284 ms for msgpackr to decode it
  in JavaScript. With three fields picked (an 8.2 MB answer), 68 ms and 18 ms. Ids alone, 4.2 ms and 0.5 ms (release
  binding, median of seven, 2026-10-28, a load of 11).
- **apack's views read pages**: the largest thread's first 50 messages, read and decoded, take 0.68 ms.
- **No apack code holds a unit across an `await`**: no measured action wrote in a unit at all.

## Reads without a decode

**The design.** A `rows` answer may be asked for as `lazy`: the binding returns, per row, a view over the answer's
bytes — the bytes copied once out of LMDB into one buffer, never a pointer into the map — and a field is decoded when
first read, then cached. Copying out is what makes it safe: LMDB's pages are valid only while the read transaction is
open, and a view outliving it would read freed or rewritten pages. An epoch scheme that keeps the transaction while a
view lives is rejected: one forgotten view would pin a snapshot and grow the file.

**API.** `terminal: { answer: "rows", lazy: true }`; in apack, `pickAll({ lazy: true })` and `pick(fields, { lazy:
true })`, answering rows whose fields are getters over the buffer. A lazy row is frozen, as reads are where reads are
frozen.

**When.** A view whose rows are mostly unread: a list that renders a title of a large row. apack's views pick their
fields (three fields of every message decode in 18 ms), so the first move is always to pick; lazy rows are for what
picking can't express, and wait for a view measured to decode fields it doesn't read.

## Units that span an `await`

**The design.** A unit reads from a snapshot and buffers its writes in JavaScript; its commit takes the write
transaction, checks that nothing it read has changed since its snapshot — the key ranges tracked reads already record
(`../architecture/engine.md`, Change sets and watch) — and applies the writes, or refuses with `CONFLICT`, naming a
range that changed, for the caller to retry. FoundationDB's model. A unit that wrote nothing commits nothing.

**API.** `db.beginOptimistic(): OptimisticUnit` with `read`, `write` (buffered, refused only at commit) and `commit()`;
in apack, `transactAsync(async (tx) => …, { retries })`. Unlike `transact`, a refusal is thrown at commit, not at the
write that caused it, which is why it is a separate call and not a mode of `transact`.

**When.** A flow that must read, await a model call and write as one unit. None does: apack's flows write their run
records as they go. It waits for that flow, and for a reading of conflict rates under apack's real writes, which decide
whether retries are rare.

## Risks

- **A lazy row's buffer** holds the whole answer while any row of it is alive; a caller keeping one row of a large
  answer keeps all of it. Answered by documenting it and by `pick`, which copies.
- **An optimistic unit's read set** must be every range it read, including a query's index ranges; a read the tracking
  misses is a conflict missed. The tracked-reads specs are the proof, extended to units.

## Phases

1. **Lazy rows.** Triggered by a view measured to decode fields it doesn't read for over 10% of its run. Done when:
   `lazy` rows answer exactly what eager rows answer (the model harness's plans compared field by field); a row
   outliving its read stays valid, shown by a spec reading it after later commits and a `rebuild()`; the 148 MB read
   above is taken again, reading one field of each row.
2. **Optimistic units.** Triggered by a flow that holds a unit across an `await`. Done when: a unit whose read range
   another commit wrote is refused with `CONFLICT` naming it; one whose ranges are untouched commits its writes; the
   model harness runs workloads of interleaved optimistic units against a serial model; conflict rates are recorded
   for apack's flows.
