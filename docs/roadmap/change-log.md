# The change log as a product

Status: idea.

## Where it starts

Every commit already produces a change set with a sequence that only increases (`../architecture/engine.md`, Change sets
and watch). Today it is delivered to listeners and forgotten.

## The idea

Keep it: an append-only log of what each commit wrote, primary data only, compacted behind a retention point. Three
features become readers of one stream rather than three mechanisms:

- **Incremental backups.** A backup is a full copy at a sequence plus the log after it; taking the next one copies
  only what changed. A restore replays to any retained sequence, which is also a point-in-time recovery.
- **Sync across devices.** Two databases exchange the log entries the other lacks. Concurrent edits to one field need a
  rule — last writer by a hybrid logical clock, or a merge per field type — and relations and short codes need their
  own (a code numbered on two devices).
- **Undo.** A user's action is a range of the log; undoing it writes the inverse of each entry, as a new commit, so
  undo is itself undoable and syncs like any write.

## In what order

The commit sequence, which tracked reads added, is the log's foundation. **Incremental backup is the first use**: it
needs only the log and a restore that replays it, and apack already has a backup screen to put it behind. **Sync is a
product decision before it is an engine one** — which devices, which data, what a conflict looks like to a user — and
undo follows what apack decides an action is.


- The log's size against the database's, and what retention a desktop app can afford.
- Whether sync belongs in elmdb or in a layer above that reads the log.
- Identity across devices: ids are minted per process today and collide only by chance; short codes collide by design.
