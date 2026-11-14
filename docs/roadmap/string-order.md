# A string's order read from its index

Status: designed, 2026-11-13, and not built. It is the part of roadmap change 5 still open (`../roadmap.md`): an
`orderBy` on a string field reads every entity of the set, where one on a number reads ten entries for its first ten.
It adds derived keys, so it is built before format 1 is frozen (change 8). The design was read against the code by a
second reader, and what that changed is under What was set aside.

## Where it stands

A value index files a string under its own bytes (`../architecture/format.md`, `d/x`), which is what an equality and
`unique` need. An `orderBy` and a `range` with string bounds order strings by the schema's collation, ICU's root or a
locale's (`../architecture/schema.md`, `collation`), and the collation's order is not the bytes' order: `a` sorts
before `B`, and `é` beside `e`. So an order on a string field reads the field of every entity in the set and compares
them with the collator (`engine/reads.rs`, the `orderBy` step's sorted arm), and a string `range` tests each entity it
is given. With a `limit` straight after it the order keeps only that many as it goes (`first_ordered`), one pass and
no sort; without one it sorts them all. Either way every entity's field is read.

An index holding any value it can't file, a string past 256 bytes among them (`MAX_INDEXED_STRING`), is read for no
order at all, of numbers either: one such value sends every order on the field to the sorted arm.

A change of the schema's collation is applied at once and costs nothing, because no stored key depends on it
(`../architecture/schema.md`, Changing a schema).

## The measurements

Two runs of disposable programs on a copy of `main` at `0933e43` (Apple M1 Pro, 2026-11-13, load 14.4 for the first,
built in release, `icu_collator` 2.4.0). No database is in them: these are the collator's costs alone.

The first, root collation, a million titles of three to six words from a vocabulary of 4,000, a tenth of the words
with an accented letter and a quarter capitalized, 33.6 bytes a title:

| What | Reading |
|---|---|
| A sort key | 40.2 bytes a title, 1.2 times the string; 775 ns to make |
| The million sorted by the collator's comparison | 2,351 ms |
| The million sorted by their sort keys' bytes | 211 ms, the same order |
| The ten least kept in one pass of comparisons, which is what a `limit 10` costs today before its fields are read | 335 ms |
| `é` composed and decomposed | equal to the collator, and one sort key |
| `a` and `A` | different sort keys |

The second, sort keys at their longest and against the collator's comparison:

| What | Reading |
|---|---|
| 256 uppercase ASCII letters | a key of 522 bytes, past LMDB's 511-byte key |
| 256 bytes of capitalized words | 368 bytes |
| 85 of U+FDFA, 255 bytes | 3,620 bytes |
| A key written into a buffer too short for it | an error, and for 128 Cyrillic letters the bytes written were not the whole key's first |
| The sign of the comparison against the keys' byte order, 300,000 pairs of random strings of mixed scripts in each of root, `sv`, `de`, `fr-CA`, `ja`, `es`, `tr`, `zh` | the same on every pair |
| Pairs equal to the collator and of different bytes, of each 300,000 | 109 to 160 |

What they say: a sort key orders as the collator does; it has no useful bound, so a key is cut and cut keys are met
in ordinary data; a cut is the first bytes of the whole key and can't be had by stopping the collator early; and two
strings of different bytes can be equal to the collator, so a sort key can't stand in for a string where an equality
is asked.

## The design

**Every value index a schema declares keeps its last field's strings in the collation's order, and nothing is
declared.** A caller who indexes a field and orders by it gets the index, without knowing there was a choice. The one
field left out is one declared a `ref`, whose strings are ids. The engine's own indexes, of short codes and roles,
are read for no order and keep none.

**The keys hold an index's order, in the form its values are held.** One more derived family, declared in `format`'s
table, with its blocks as `d/x` has `d/xp`:

| Key | Holds |
|---|---|
| `d/xo` type index build leading… sortkey | the places of the entities whose last field holds a string with this sort key, under what the index's fields before the last hold: as `d/x` holds a value's places |
| `d/xo` type index build leading… (no key) | the places of the entities the order can't place: a list, a map, any value the index files nowhere but a string |

- **It is `d/x` again with a sort key where the string was.** So an order on strings is read by what reads an order
  on numbers: the same walk, the same blocks, the same cursor of a value and a place. Strings the collator holds
  equal have one sort key and so one entry, their entities in place order, which is where the sorted arm's stable
  sort has them: there is nothing to merge.
- **The sort key** is the collator's for the schema's collation at its default strength, a bytes part, cut to the
  room its key has under LMDB's 511 bytes. The cut is the first bytes of the whole key, never the collator stopped
  early. An entry whose key fills the room may hold strings whose keys differ past it: a reader orders that entry's
  entities by reading their fields, whole sort key and then place. Every other entry is read without a row.
- **A string of any length is filed**, the 256 bytes `d/x` stops at being an equality's limit and not an order's. A
  long string no longer takes the index away from an order.
- **What the order can't place has its own entry**, so whether an index can be read for an order is whether that
  entry is there, and doesn't depend on what `d/xu` holds for equalities. Phase 4 reads it where it reads no order
  today.
- **`build`** is a number the index's built marker holds. Entries under any other are no index's, and are deleted in
  the background. It is what lets a rebuild fill an empty range, below.
- **One function derives an entity's entries for an index, both families**, and a write, the builder, `rebuild()` and
  `check()` each file what it gives. An entity filed under `d/x` and not here would be missing from an order's
  answer, so no caller files one family alone.

**What reads it**, each where the plan reads a number's entries today and under the same conditions (one type, a
built index, and for a compound index equalities on the fields before the last):

- `orderBy`: false, true and the numbers from `d/x`, then the strings from `d/xo`, then the entities without a value
  from `d/xw`; backwards when descending. A field holding both numbers and strings is read across the seam.
- `after`, a page's cursor: a seek to the cursor's value and place, in whichever family holds it.
- `range` with string bounds: a seek to the lower bound's sort key and a walk to the upper's.
- `least` and `greatest`: the first entity the order hands on within the scope, and its field read from its row,
  since an entry may hold entities the scope leaves out and strings of different bytes.

An equality and `unique` read `d/x` as now, and depend on no sort key.

**A stale sort key is never read.** A sort key is the collator's output, and ICU's documentation says to presume
stored ones invalid after an update of CLDR, of Unicode or of the library. Read stale, they give a wrong order with no
error. So:

- *The stamp* is a hash of four things: the schema's collation; a number for this layout (the cut, the room, the
  collator's options); the names and versions of every ICU crate in the collator's dependency tree, which
  `Cargo.toml` requires exactly (`=`) and `cargo xtask` holds to the lockfile; and the sort keys of a few dozen probe
  strings, made by the running collator at open. The exact requirements make another dependency that needs another
  version fail to resolve, loudly; the probes make a build with other data under the same versions (`ICU4X_DATA_DIR`,
  which the build also refuses) a different stamp and not a wrong order.
- *The marker* of a built index (`d/xb`) records, for its order: the stamp, the `build` number, and whether that
  build is whole. It is read in each transaction that uses the order, in that transaction's own snapshot.
- *A read* uses `d/xo` only under a marker whose stamp is its own and whose build is whole. Otherwise it reads the
  rows, as today.
- *A write* that changes the index's strings under a marker of its own stamp files under that marker's build, whole
  or not. Under any other stamp, or none, it first rewrites the marker in the same unit, to its own stamp, the next
  build number and not whole, and files under that. Nothing reads the keys it didn't write, and the builder is told
  once the unit commits.
- *The builder* fills a build from the rows, as it builds a value index: into a range no key was in, idempotently,
  beside writes that keep it. Each of its units checks that the marker still names its build and stops if not; the
  last marks the build whole. It starts once the marker has stood unchanged for a time, so two elmdb versions
  writing one database leave it unbuilt and sorted while both write, and don't rebuild after each other without end.
- *An open* that can write compares each index's marker with its stamp where it queues unbuilt indexes, so a
  database that is only read after an update is rebuilt too. One opened read-only sorts.
- *The schema's collation* is in the stamp, so a change's effect becomes `Builds` where it is `Applied` today, by the
  same path.
- *`check()`* derives `d/xo` under a whole build of its own stamp and reports an entry missing, one too many and one
  under the wrong key, each by name. Under another stamp it says the order was filed by another collator and was not
  compared, which is what `elmdb-cli` of another build meets.

**What it costs.** For each write of a string in a declared index: one sort key, 775 ns for a title by the first
reading and more for a longer string, and one place filed a second time. An index grows by a sort key for each
distinct string and a place for each entity: for strings nearly all different, a little more than its `d/x` again;
for a field of few values, the places alone. An update of the collator's crates rebuilds every index's order once,
in the background, at the cost of building the index. Each is read in phase 2, on a unit's time and the file's size,
before and after.

## What was set aside

- **An index that asks for it (`"collated": true`).** The first draft. A caller who doesn't know to ask gets the
  rows read, silently, and the option is a path most indexes and most tests never run.
- **A directory of distinct strings, each pointing back to its `d/x` entry.** The second draft: nothing to pay for a
  write that repeats a string, and a field of few values costs nearly nothing. Read against the code, it needed a
  second read for each string, a merge by place of the strings the collator holds equal, a key for each string made
  unique by a hash, its entries written where a `d/x` key first appears and last goes, and a second kind of entry
  for strings too long for it. Filing places under the sort key needs none of those and is read by the walk already
  tested; its cost is the second place filed, which is measured before it is accepted.
- **The order built when a plan first wants it.** It costs nothing for an index never ordered. But what is stored
  would depend on which plans ran and not on the rows and the schema, a read would cause a write, a database opened
  read-only could never have it, and a caller's tests would run one arm where production runs the other.
- **An order kept in memory by each process**, as the edge mirror keeps edges. No stored key to go stale; some second
  and tens of megabytes for a million strings at every start, emptied by another process's commit.
- **`d/x` itself keyed by the sort key, then the string.** One family, exact equality kept. But every equality and
  `unique` would then wait on a rebuild after a collator update.
- **Leaving out what doesn't fit**: a string past 256 bytes, or one whose sort key passes the room. Either sends
  every order on its field back to the rows.
- **Cutting a key by stopping the collator at the room.** The bytes written are not the key's first (second
  reading).
- **Rebuilding in place**, by clearing the old keys and filing the new. A writer of the same stamp, a second
  builder and a writer of another stamp each meet a range half cleared; a build under a new number meets none.
- **A constant raised by hand when a test of pinned sort keys fails.** The test samples; the pinned versions and the
  probes are in the stamp itself.
- **Ordering by bytes and calling that the order.** It is what the index has, and it is wrong for any text a person
  reads: capitals before every lowercase letter, accented letters after `z`.

## Risks

- **The two arms must agree.** The sorted arm stays, for sets no index orders, and is what the model harness holds
  the index's order to.
- **The second place filed.** Every write of an indexed string pays it. If phase 2's reading on a unit's time is
  more than the order is worth for an index, the answer set aside for that day is a field declared to hold
  identifiers, ordered by its bytes from `d/x` with no second family, not an option on the index.
- **An entry that fills the room.** Its entities' fields are read whenever an order or a page reaches it. Strings
  that agree for some two hundred characters are few in a title and may be many in a field of paths or of log lines
  under a long leading value; EXPLAIN counts the rows it read.
- **A rebuild at every update of the collator's crates**, for every database, at its next open that can write.

## Phases

1. **The readings.** The million-note benchmark (`../architecture/testing.md`, Against SQLite) gains an `orderBy` on
   its indexed string with `limit 10`, a page of fifty near the order's end, a string `range`, and an order with no
   limit. Done when the four are recorded in `../evidence/million-notes.md` as the sorted arm answers them, with that
   index's `d/x` size and a unit's time.
2. **The keys, the stamp and the order.** `d/xo` in `format`'s table, filed through the one function by writes,
   `rebuild()` and the builder; the marker, the stamp, the exact requirements and their `xtask` check;
   `orderBy`, `after`, `least` and `greatest` reading it. Done when:
   - the four readings are recorded again, the first ten of a million reading ten entries where no entry is trashed,
     and the index's size and a unit's time beside phase 1's;
   - a property test holds the sign of the collator's comparison to the sort keys' byte order, over random strings
     and the tailored collations of the second reading;
   - the model harness orders by an indexed string with a filter before the order, `after` both ways inside an entry
     of equal strings and inside one that fills the room, a compound index, the trash, a field of numbers and
     strings, strings past 256 bytes, and a read inside a unit;
   - `check()` reports a `d/xo` entry removed, one added and one moved, each by hand, and a copy of the code with one
     caller filing `d/x` alone fails it;
   - a database filed under another collation with its stamp forged to this one's is shown ordered wrongly, which
     proves the test's keys differ, and the same database with its true stamp orders by the rows until the builder
     ends and by the index after; a database whose collation changed does the same;
   - the builder's concurrency model (`builder/model.rs`) runs a write of another stamp between two of a build's
     units, and two builders of one build;
   - a watch on an ordered read is told when a commit files a string before the first it read.
3. **Ranges.** A string `range` reads `d/xo`. Done when the benchmark's range reads no entity outside its bounds but
   those of an entry that fills the room.
4. **No value takes the index away.** The entities the order can't place are read from their rows and sorted among
   themselves, where the sorted arm has them, so an index holding a list or a map is still read for an order, of
   numbers too. Done when the model harness holds it, and an order's cost with a thousand such values among a
   million is recorded.
