# One builder for a database's life

Status: designed 2026-11-18, not built. It is a change to how the engine is put together, with nothing in it a
consumer sees but fewer wrong answers. It should land before the next change that gives the builder a new kind of
work.

The index builder is a thread that builds what a schema declares over the entities that predate it, and drains a
rank index's pushes (`engine/builder.rs`). Today a database stops its builder and starts another whenever its schema
changes, and keeps the one it has in a slot that is empty between the two. This note proposes one builder from open
to close, handed each new schema, and says what that deletes.

## Where it starts

What the stop and start has cost, by the pull request that found it:

| Found in | Defect |
|---|---|
| #97 | A close that gave up left the database open with no builder, and what was unbuilt queued nowhere |
| #99 | A schema change made by a listener on the builder's thread joined that thread from itself |
| #99 | A close asked for on the builder's thread waited for a store that thread holds |
| #99 | A close after a schema change by the same listener still hung: the refusal asked after the builder the database had by then |
| #99 | A wait for the builds on the builder's thread waited for itself |
| #99 | The listener of the builder's commits was let go of outside its callback, and the last of a database closed there |
| #182 | A wait for the builds answered `Ok` when a schema change stopped the builder it waited on |
| #182 | A wait asked for between the two builders answered `Ok`, the slot being empty |
| #182 | A wait asked for as a close gave up answered `Ok` before the builds were queued again |
| #182 | A close held its mark past the store's close, which a listener on the store's wake thread would have deadlocked (found in review, before it landed) |
| #182 | An index queued by two that found it owed was built twice |

Two more are known and left (#182's description): a wait made as this process's own `reset` or `replace_with`
commits can be refused `DATA_REPLACED`, and one made as another thread adopts a replacement can answer `Ok` before
the adoption has queued what it left unbuilt.

Each fix added state to `database.rs`. At 367865a (2026-11-18) it holds, for the builder alone: the slot
(`Mutex<Option<Builder>>`), `Replacing` (a count of the schema changes and closes in progress, and a condition
variable), `Builders` (what a commit starts a builder through), a loop in `set_schema` that takes a builder, lets go
of the schema's lock, joins the thread and takes the lock again, a second loop in `close_as` that stops any builder
started after it stopped the first, and in `wait_for_index_builds` a loop over three states of the slot.

## What is wrong with the shape

- **The builder's identity changes.** A wait, a refusal that asks whether the caller is on the builder's thread, and a
  queue each name "the builder", which is a different object after every schema change and none in between.
- **An empty slot means three things**: nothing to build, a builder being replaced, and a database closed. Each
  caller has to tell them apart, and #182's three early `Ok`s were three callers that didn't.
- **A schema change waits for a thread.** It has to let go of the schema's lock to do so, since the builder's commit
  may be delivering to a listener that takes it. Every window above opens there.
- **The part with the defects is the part with no model.** `engine/builder/model.rs` holds the builder's `Core`, its
  queue, its pauses and its waiters, under Shuttle, and says so of itself: "`database.rs`'s slot the builder is kept
  in" is outside it (`../architecture/testing.md`). None of the eleven defects was in `Core`.

## What the code already does

Read on 2026-11-18, at 367865a:

- **A chunk built to an earlier schema can't commit.** `builder_commits` (`engine/mod.rs`) validates the builder's
  schema against the stored fingerprint inside the write transaction, before the chunk writes
  (`schema.validate(txn, db)`), and a schema change stores the new fingerprint in the transaction that applies it
  (`apply_schema`, `database.rs`). LMDB has one writer, so a chunk commits wholly before the change or is refused
  after it. `engine/tests/schema_versions.rs` holds this
  (`a_builder_with_an_earlier_declaration_refuses_before_writing_index_entries`).
- **So does every read the builds plan from.** `build_order`'s read and each of `build_rank`'s, the one that asks
  whether the index owes work and the one that plans a drain outside the write lock, validate the same way. A drain
  planned to one schema and applied after a change is refused at its commit. `build_index` asks whether its index is
  built with no validation, and its chunk's commit then validates.
- **So stopping the builder is not what keeps a schema change sound.** It is there so that the old builder's work
  isn't listed as failed, and so that what it had left is queued for the new one.
- **A close does need the thread to end**: the thread holds the store, which a close takes back whole. A close asked
  for by a listener is refused (`CALLBACK_ACTIVE`), so it never joins its own thread. At exit the thread is left
  unjoined (`Builder::abandon`), since on Windows the exit handlers run after every other thread was ended.
- **Nothing but a schema change and a close stops a builder.** `reset`, `replace_with`, `rename_type`, `purge`,
  `rebuild`, a load and `adopt_replacement` commit beside it, and queue what they leave owed.

## The design

One `Core` for a writable database, made at open and kept until the database is dropped. Its thread starts with the
first work queued, as a builder does now. The slot holds it always: there is no empty state.

1. **Work is run for a generation.** The `Core` holds the schema it builds to and a count of how many times that was
   replaced. The thread takes a piece of work with the schema and the count as they stand, and builds to them.
2. **A schema change retargets, and waits for no thread.** Holding the schema's lock for writing throughout, it tells
   the `Core` a change has begun (the count moves, the queue and what was put off are emptied), applies the schema,
   and tells it the change has ended with the schema now held and what that leaves unbuilt. Where applying failed,
   that is the schema as it was and what is still unbuilt under it, which is what `interrupted` carries today.
3. **Work of an earlier generation is let go of, not failed.** A piece of work that ends, or is refused, after the
   count has moved is dropped as work over replaced rows is dropped now (`DATA_REPLACED`). The count moves before the
   fingerprint is stored, so a refusal the change itself caused is always of an earlier generation. A fingerprint
   another process stored is of the current one, and fails the build as it does today.
4. **The wait is one condition on one object**: no work queued, none running, no change begun and not ended, or the
   `Core` closed or held up. It has no loop over builders. What #182 added last stays, since it makes `Ok` something the
   database says: an idle `Core` reads what is unbuilt and queues it.
5. **A close tells the `Core` it is closing**, which ends the thread after its piece of work and refuses work queued
   meanwhile, and joins it as now; at exit it leaves it unjoined as now. A close that gives up tells the `Core` it is
   open again with what is unbuilt, which starts a thread. The loop that stops a builder started after the first goes:
   work queued to a closing `Core` isn't taken.

What this keeps as it is: a wait asked for on the builder's thread is refused (`CALLBACK_ACTIVE`), now by a thread
that is the builder's for the database's life; a build's chunks, its units, and what it waits out; the back-off of an
order's build, which is per builder and so stops being reset by every schema change.

### Why each window closes

- **A listener on the builder's thread changes the schema.** It takes the schema's lock, which its own thread doesn't
  hold (a watch's read lets go of it before the listener runs), retargets, and returns. The thread's piece of work is
  of an earlier generation and is dropped. There is no thread to join, so no case for "its own".
- **A wait as the schema changes.** The change holds the `Core` in "a change has begun" from before it applies until
  it has queued what is left. The wait's condition is false throughout, whichever came first.
- **A commit that queues work as the schema changes.** It queues to the same `Core`. Queued before the change began,
  its work is emptied with the rest and what is unbuilt is read again after; queued after, it is of the new
  generation.
- **A unit begun while the change waits for the builder** (`engine/tests/schema_versions.rs`). The change no longer
  lets go of the schema's lock, so there is no such moment.

### What goes

`Replacing` and `Replace`; `Builders`' starting of a builder; the slot's `Option`; `Builder::pending`; the take,
unlock, join and lock loop in `set_schema`; the loop in `close_as` that stops later builders; the three-state match
and the retry in `wait_for_index_builds`; `Waiter::stopped`. `Builder::stop`'s case for its own thread goes with the
join.

## The model first

`Core` gains three operations: a change begins, a change ends with a schema and work, and closing. Each is modelled
under Shuttle before `database.rs` calls it, beside the properties the model holds now:

- A waiter returns `Ok` only at a moment when no work of the current generation is queued or running and no change is
  begun and not ended.
- No piece of work is reported done or failed for a generation it wasn't taken in.
- Work handed over at a change's end runs, once, after it.
- A change begun always ends the waiters' wait eventually: by its end, or by a close.
- A close ends the thread, the waiters and every pause, whatever the `Core` is doing.
- A listener's change made on the builder's thread, modelled as a change begun and ended by the task that is running
  a piece of work, ends with that task taking the new generation's work.

Each property is shown failing once on a mutated copy of the model's `Core`, as every check is.

## Phases

Each is a pull request, with its gate.

1. **The model.** `Core`'s new operations and their properties, with no caller. Gate: the properties above, each
   shown failing; the existing model's unchanged.
2. **A schema change retargets.** `set_schema`, `Builders::queue`, the wake listener, `adopt_replacement` and
   `wait_for_index_builds` over the one `Core`. Gate: the engine's suite, with `engine/tests/tracked.rs` and
   `schema_versions.rs` rewritten only where they assert on the slot; and `database.rs` and `engine/builder.rs`
   together shorter than at 367865a outside their tests. If they aren't, the design is wrong and this stops.
3. **A close closes the `Core`.** The second loop in `close_as` and the rest of `Replacing` go. Gate: as phase 2.
   This one is optional: phase 2 removes what the defects came from.

## Not decided

- **Whether the thread should hold the store at all.** Taking it for each piece of work would let a close stop
  joining, and the wait in `close_as` for the store's other holders would cover the builder with them. It changes
  what a close at exit leaves running, so it is left out of phase 3 until that is read on Windows.
- **A failure of work whose index the new schema still declares.** Today a schema change starts a builder with no
  failures, so an index that couldn't be built is tried again. The design keeps that: a change empties the failures.

## Decided since

- **Whether `reset`, `replace_with` and `adopt_replacement` begin a change too.** They don't. A build of an index kept the place it had reached in a variable of the builder's, and went on from it
  over rows a `replace_with` or a rename had put there: an index those rows couldn't be filed in was marked built,
  and one such answered a read with no entity (`engine/tests/derived.rs`, the two tests of a build under way as every
  row is replaced). The place is now the store's (`d/xbr`, `../architecture/format.md`), deleted by whatever says
  whether the index is built or replaces its entries, so a build after any of them begins at the first row,
  whichever process made the change, and no operation has to tell the builder. A rank index's join keeps its place
  the same way and was read as sound: a full rebuild builds a rank index whole in its unit, so a join that goes on
  finds every node joined. Under this design that leaves the count to schedule by, not to be correct by: work of an
  earlier generation is let go of so that it isn't listed as failed, and would be refused or harmless if it ran.
  The two races #182 left are a wait's, not a build's, and stay with invariant 4.

## What this is not

It adds no work to the builder and changes no build. It doesn't touch the store's own threads, group commit or the
wake listener's. The text index's journal (`text-index-at-scale.md`) is the kind of change it is meant to come
before: one more kind of work the builder files, and one more path through every operation above.
