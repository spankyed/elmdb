# The change log as a product

Status: designed, 2026-10-28: incremental backup as a set of what changed since a sequence, not a log of writes, which
G8's data shows a log can't afford (`../evidence/next-phase-10.md`, The change log). Sync and undo are product
decisions above it, named here and not designed.

## Where it starts

Every commit produces a change set with a sequence that only increases (`../architecture/engine.md`, Change sets and
watch): the entities and fields it wrote, the relations it changed. It is delivered to listeners and forgotten. A
backup (`elmdb backup`, apack's backup screen) is a compacted copy of the whole database.

## The measurements

- **A log of writes grows with the square of a streamed message.** apack writes a streamed reply's whole `text` on
  every chunk and its tool-activity block whole as each entry starts and ends (`updateMessageState`, default-setup's
  threads repository; Phase 3 counted 181 writes for a 400-word reply). Replayed over G8's 15,757 assistant messages,
  a log of those writes holds about 8.2 GB for 142 MB of final values, 57 times what it describes; most of it is the
  tool-activity blocks, one message's 57 entries holding 3.3 MB.
- **What changes in a day is small.** G8's messages span 96 active days from 2026-04-17 to 2026-09-04; the final values
  a day added are 0.86 MB at the median and 8.8 MB on the busiest day, against a compacted file of 122 MB.

So a log kept between backups is the expensive way to know what changed: a day of chat could log half a gigabyte for
nine megabytes of change. What an incremental backup needs is which fields changed since the last one, read at backup
time.

## The design

### What changed since a sequence

The store keeps, for each entity field and relation, the sequence of the last commit that wrote or removed it:

- `l/s id field → seq` — the field's last change (a removed field keeps its key, its removal being a change);
- `l/q seq id field → nothing` — the same, ordered by sequence, so "everything changed after `n`" is one range read;
- `l/r seq relation → nothing` and `l/t relation → seq` — a relation's last change, alike;
- an entity's destruction is a change of each of its fields and of its record.

A write replaces its field's `l/q` entry (reading `l/s`, which the write's undo log reads anyway, Phase 7's
`put_over`); a field written a hundred times in a stream holds one entry. The two keys are primary bookkeeping, not
derived data: `check()` can't derive them, and `rebuild()` leaves them.

**Retention.** `l/q` and `l/r` entries at or before the oldest sequence a caller still needs are dropped by
`db.forgetChangesThrough(seq)`, which apack calls after a full backup is verified. Until a caller asks, nothing is kept
(the store's option `changes: "kept"`, off by default), so a database nobody backs up incrementally pays nothing.

### Incremental backup

`db.changesSince(seq)` answers, in one snapshot, the current value of every field changed after `seq`, the fields
removed, the relations changed or removed, and the snapshot's sequence. A backup is a full copy at a sequence plus a
chain of such sets; a restore loads the full copy and applies each set in order, as one unit each (`elmdb load`
extended with `--apply`). Restoring to a retained sequence other than a backup's is not offered: the set holds the
latest value, not the history.

### API

- `OpenOptions.changes: "off" | "kept"`; `Database.changesSince(seq): ChangeSince`, `forgetChangesThrough(seq)`;
- the CLI's `elmdb backup --since <seq> <db> <file>` and `elmdb load --apply <file>`;
- apack: the backup service takes an incremental backup after the first full one, and the backup screen lists the
  chain.

### Sync and undo

Both need history, which this design deliberately doesn't keep. **Sync** is a product decision first (which devices,
which data, what a conflict looks like to a user) and then an identity one (ids minted per process, short codes that
collide by design). **Undo** depends on what apack decides an action is; written as inverse operations applied as a new
commit, it needs the values a user's action replaced, which a unit's undo log holds while it runs and could hand to the
caller. Neither is designed here.

## Risks

- **A write's cost.** Two more key writes per field write, of keys the write's own page is likely to share; measured
  against apack's bench (a create, a page read after writes) before it is on by default.
- **Missed changes.** A write path that forgets its bookkeeping silently drops a change from every backup after it.
  The model harness gains a property: after any workload, `changesSince(n)` applied to a copy at `n` equals the
  database.
- **A restore across a schema change** applies values to fields the new schema may not declare; a set records the
  schema it was taken under, and `load --apply` refuses a set from another schema version.

## Phases

1. **The changed set.** Done when: with `changes: "kept"`, every write path (field, delete, destroy, relation, trash,
   rename, purge) keeps `l/s`/`l/q` and `l/t`/`l/r`; the model harness's property above holds for every workload;
   `forgetChangesThrough` drops exactly the entries at or before its sequence; apack's bench with the option on is
   within 10% of off.
2. **Incremental backup.** Done when: `elmdb backup --since` and `load --apply` restore G8's copy from a full backup and
   two sets taken after writes, equal by `elmdb inspect` and a dump compare; apack's backup screen takes and restores
   a chain; a set's size is recorded for a chat turn and a busy day's writes.
3. **History**, for undo or sync, each its own decision once apack decides what an action or a device is.
