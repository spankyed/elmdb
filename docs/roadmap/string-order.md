# A string's order read from its index

Status: designed, 2026-11-13, and not built. It is the part of roadmap change 5 still open (`../roadmap.md`): an
`orderBy` on a string field reads every entity of the set, where one on a number reads ten entries for its first ten.
It adds derived keys and raises the format's version, so it is built before format 1 is frozen (change 8). This is
the fourth draft: each of the three before was read against the code by a second reader, and what that changed is
under What was set aside.

## Where it stands

A value index files a string under its own bytes (`../architecture/format.md`, `d/x`), which is what an equality and
`unique` need. An `orderBy` and a `range` with string bounds order strings by the schema's collation, ICU's root or a
locale's (`../architecture/schema.md`, `collation`), and the collation's order is not the bytes' order: `a` sorts
before `B`, and `é` beside `e`. So an order on a string field reads the field of every entity in the set and compares
them with the collator (`engine/reads.rs`, the `orderBy` step's sorted arm), and a string `range` tests each entity it
is given. With a `limit` straight after it the order keeps only that many as it goes (`first_ordered`), one pass and
no sort; without one it sorts them all. Either way every entity's field is read.

An index holding any value it can't file (`d/xu`), a string past 256 bytes among them (`MAX_INDEXED_STRING`), is read
for no order at all, of numbers either: one such value sends every order on the field to the sorted arm.

A change of the schema's collation is applied at once and costs nothing, because no stored key depends on it
(`../architecture/schema.md`, Changing a schema).

## The measurements

Three runs of disposable programs on a copy of `main` at `0933e43` (Apple M1 Pro, 2026-11-13, load 14.4 as the first
and the third began, built in release, `icu_collator` 2.4.0). No database is in them: these are the collator's costs
alone.

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
| The sign of the comparison against the keys' byte order, 300,000 pairs of random strings of mixed scripts in each of root, `sv`, `de`, `fr-CA`, `ja`, `es`, `tr`, `zh` | the same on every pair |
| Pairs equal to the collator and of different bytes, of each 300,000 | 109 to 160 |

The third, a key cut as it is made, by a sink that keeps the bytes up to a room and refuses the next
(`CollationKeySink`, behind the crate's `unstable` feature):

| What | Reading |
|---|---|
| The bytes kept against the whole key's first, and whether the collator stopped exactly when the key passed the room: 200,000 cuts (40,000 random strings of up to 300 characters, rooms of 1, 8, 64, 200 and 450 bytes) in each of root, `sv`, `fr-CA`, `ja`, `th`, `lt`, `ko`, `de-u-kn`, `en-u-kf-upper`, `de-u-co-phonebk`, `zh-u-co-pinyin` | the same on every cut |
| A string of 10,000 bytes | its whole key, 14,306 bytes, in 432 µs; cut at 450 bytes in 21 µs |

What they say: a sort key orders as the collator does; it has no useful bound, so a key is cut, and cut keys are met
in ordinary data; the cut is exact and cheap when the collator is stopped by its sink; and two strings of different
bytes can be equal to the collator, so a sort key can't stand in for a string where an equality is asked.

## The design

**Every value index a schema declares keeps its last field's strings in the collation's order, and nothing is
declared.** A caller who indexes a field and orders by it gets the index, without knowing there was a choice.
Whatever the field's declared type: what is filed doesn't change when a declaration does. The engine's own indexes,
of short codes and roles, are no schema's, are read for no order, and keep none.

**The keys hold an index's order in the form its values are held.** Three derived families, declared in `format`'s
table:

| Key | Holds |
|---|---|
| `d/xo` type index build leading… sortkey cut | the places of the entities whose last field is a string with this sort key, under what the index's fields before the last hold: as `d/x` holds a value's places, in blocks past 256 (`d/xop`) |
| `d/xob` type index | the order's marker: the stamp it was filed under, its build, and whether the build is whole |
| `d/xl` type index leading… | the places of the entities whose last field is a string past 256 bytes, which `d/x` has no entry for and `d/xu` held until now |

- **`d/xo` is `d/x` again with a sort key where the string was.** So an order on strings is read by what reads an
  order on numbers: the same walk and the same blocks. Strings the collator holds equal have one sort key and so one
  entry, their entities in place order, which is where the sorted arm's stable sort has them: there is nothing to
  merge.
- **The sort key** is the collator's for the schema's collation at its default strength, a bytes part, made by a
  sink that keeps the bytes the key has room for under LMDB's 511 bytes and stops the collator at the next. The part
  after it says which: `false` for a whole key, `true` for one that was cut. A cut entry may hold strings whose keys
  differ past the cut: a reader orders its entities from their rows, by whole sort key and then place. Every other
  entry is read without a row.
- **Only a field that is a string is filed**, never the strings of a list, which `d/x` files for an equality and no
  order places.
- **A string of any length is filed**, the 256 bytes `d/x` stops at being an equality's limit and not an order's.
  `d/xl` says, with no sort key in it, that an index holds such strings.
- **A string whose key has no room is one the order can't place.** The room is what the type's and index's names,
  the build and the leading values leave. Under 64 bytes of it, the entity is filed in `d/xu` for that order, and
  in `d/x` as now.
- **`build`** is the commit sequence of the unit that began the build, so no two builds of a database share one.
- **One function derives an entity's keys for an index, every family**, and hands them on as one set, so no caller
  can file a family alone. A write, the builder, `rebuild()` and `check()` file what it gives
  (`ValueIndex::files`, which they call today).
- **The format's version is raised with these keys**, so an elmdb from before them refuses the file where it would
  have written strings and filed no order.

**What reads it**, where the plan reads a number's entries today (one type, a built index, nothing in `d/xu`, and
for a compound index equalities on the fields before the last):

- An index that holds no string, neither in `d/x` nor in `d/xl`, is read as today and reads no marker: an order on
  numbers depends on no collator.
- One that holds strings is read when its order's marker is this process's and whole: false, true and the numbers
  from `d/x`, the strings from `d/xo`, the entities without a value from `d/xw`; backwards when descending.
  Otherwise the rows are read, as today.
- `after`, a page's cursor, is an entity, its value read as the page is: a seek to that value's entry and its place.
  In a cut entry the entry's rows are read again for each page.
- `least` and `greatest`: the first entity the order hands on within the scope, and its field read from its row,
  since an entry may hold entities the scope leaves out and strings of different bytes.
- `range` with string bounds (phase 3): a seek to the lower bound's sort key and a walk to the upper's, the entities
  of a cut entry at either bound tested from their rows.
- The marker is read as a recorded read, so a watch on an ordered read is run again when the order's build changes
  and the keys it read are no longer the ones written.

An equality and `unique` read `d/x` as now, and depend on no sort key.

**A stale sort key is never read.** A sort key is the collator's output, and ICU's documentation says to presume
stored ones invalid after an update of CLDR, of Unicode or of the library. Read stale, they give a wrong order with no
error. The rules, each unit reading the marker in its own transaction and through the unit, so a savepoint rolled
back restores it:

- *A read* uses `d/xo` under the marker's build only when the marker's stamp is its own and the build is whole.
- *A write* that changes an index's strings files under the marker's build when the marker's stamp is its own, whole
  or not. Under a marker of another stamp it deletes the marker, once, and files nothing; with no marker it files
  nothing. So no build is whole that lacks a place, whoever wrote.
- *The builder* builds an order as work of its own, apart from the index's. It goes on with a build of its own stamp
  that isn't whole, and otherwise begins one: a marker of its stamp, its unit's sequence, not whole. It fills from
  the rows as it builds a value index, into a range no key was in, idempotently, beside the writes of its stamp that
  keep it. Each of its units checks that the marker is still its stamp and its build, not whole, and stops if not;
  the last marks it whole. An order another stamp's write ended is tried again after a wait that doubles, so two
  elmdb versions writing one database leave it unbuilt, and read from rows, while both write.
- *What tells the builder*: an open that can write, for every declared index whose marker is not its own and whole;
  and a unit that deleted a marker or found none, once it commits.
- *`rebuild()`* files every order whole under a new build, in its unit. *A dropped index* has its `d/xo`, `d/xl` and
  marker cleared in the unit that clears its `d/x`.
- *Keys under a build that is not the marker's* are no order's. The builder deletes them in units of their own, each
  reading the marker again; they sort before the marker's build, so finding none is one seek.
- *The schema's collation* is in the stamp, so a change's effect becomes `Builds` where it is `Applied` today, by the
  same path.

**The stamp** is a SHA-256 of: the schema's collation; a number for this layout (the collator's options, the cut,
the room), raised with any change to the function that derives a key and held by a test of pinned key bytes; and the
name and version of every ICU crate in the collator's dependency tree. Each of those crates is a direct dependency
required exactly (`=`), which `cargo xtask` holds to the lockfile and to the stamp's list, so a consumer of the Rust
crate resolves the same versions or fails to resolve. The build refuses `ICU4X_DATA_DIR`. A `[patch]` or a vendored
copy can still change what a version means; for that, the stamp also takes the sort keys of a few dozen probe strings
made by the running collator, in the schema's collation, when a database is opened. The probes are a sample and a
backstop, not the proof. The collator is made once for a collation in a process and kept.

**`check()`** derives `d/xo` under a whole build of its own stamp, and `d/xl`, and reports an entry missing, one too
many and one under the wrong key, each by name. Under another stamp it says the order was filed by another collator
and was not compared, which is what `elmdb-cli` of another build meets. `indexStatus()` and the wait for index
builds cover an order as they cover an index.

**What it costs.** For each write that changes an indexed string or the values before it: the marker read, one sort
key (775 ns for a title by the first reading, 21 µs at most for a long string by the third), and one place filed a
second time. An index grows by a sort key for each distinct string and a place for each entity: for strings nearly
all different, a little more than its `d/x` again; for a field of few values, the places alone. An update of the
collator's crates builds every index's order again, once, in the background. Each is read in phase 2.

**The decision the readings make.** If phase 2 shows a unit that writes strings nearly all different, the
benchmark's ids, more than a tenth slower, or its file more than a tenth larger, a field type for identifiers is
built before the freeze: its strings are ordered by their bytes, from `d/x`, with no second family. It is a
declaration of what the field means, in the schema, so what is stored still follows from the rows and the schema
alone.

## What was set aside

- **An index that asks for it (`"collated": true`).** The first draft. A caller who doesn't know to ask gets the
  rows read, silently, and the option is a path most indexes and most tests never run.
- **A directory of distinct strings, each pointing back to its `d/x` entry.** The second draft: nothing to pay for a
  write that repeats a string. It needed a second read for each string, a merge by place of the strings the collator
  holds equal, a key for each string made unique by a hash, entries written where a `d/x` key first appears and last
  goes, and unbounded values for cut keys. Both readers of the third draft held it rightly set aside.
- **The order built when a plan first wants it.** It costs nothing for an index never ordered, and the second reader
  held it the one rival worth weighing again at the decision above. Against it: what is stored would depend on which
  plans ran and not on the rows and the schema, a database opened read-only could never have it, and a caller's
  tests would run one arm where production runs the other.
- **An order kept in memory by each process**, as the edge mirror keeps edges. No stored key to go stale; some second
  and tens of megabytes for a million strings at every start, emptied by another process's commit.
- **`d/x` itself keyed by the sort key, then the string.** One family, exact equality kept. But every equality and
  `unique` would then wait on a build after a collator update.
- **The marker's state in the index's own built marker (`d/xb`).** The third draft. That key's presence is what says
  `d/x` is built: a write that set it for an order would have had an unfilled `d/x` read by equalities and trusted
  by `unique`.
- **A write that takes the order over for its own stamp.** The third draft. Two versions writing in turn would each
  begin a build at every turn. A write of another stamp ends a build and begins none.
- **A build numbered from one.** The third draft. A dropped index's count starts again over keys not yet deleted,
  and a `rebuild()` by another elmdb could refill a number a builder still held.
- **Whether an index can be read for an order kept under the build.** The third draft. An order on numbers would
  have waited on the collator's stamp, which it has nothing to do with.
- **Leaving out a field declared a `ref`.** The third draft. A field declared a string, then a `ref`, then a string
  again would have a whole build its `ref` years never filed.
- **The entities an order can't place read from their rows where they belong.** The third draft's fourth phase.
  They don't belong in one place: a time sorts between the numbers and the strings, a list after them, a unique
  index's list before any, and a range keeps a list holding a value in its bounds. It is another design.
- **Cutting a whole key after making it**, as the third draft had it, on a reading of the collator's slice sink,
  which drops what doesn't fit and goes on. A sink of the engine's own stops it exactly (third reading).
- **Leaving out what doesn't fit**: a string past 256 bytes, or one whose sort key passes the room. Either sends
  every order on its field back to the rows.
- **Building in place**, by clearing the old keys and filing the new. A writer of the same stamp, a second builder
  and a writer of another stamp each meet a range half cleared; a build under a new sequence meets none.
- **Ordering by bytes and calling that the order.** It is what the index has, and it is wrong for any text a person
  reads: capitals before every lowercase letter, accented letters after `z`.

## Risks

- **The two arms must agree.** The sorted arm stays, for sets no index orders, and is what the model harness holds
  the index's order to.
- **The second place filed**, on every write of an indexed string. The decision above is what answers it.
- **A cut entry.** Its entities' fields are read whenever an order or a page reaches it. Strings that agree for some
  two hundred characters are few in a title and may be many in a field of paths or of log lines; a plan that walks
  the order counts an entry's places before it reads its rows, and EXPLAIN counts the rows read.
- **A build at every update of the collator's crates**, for every database, at its next open that can write.
- **The sink is the crate's `unstable` interface.** It is exempt from the crate's versioning, which the exact
  requirements make safe to depend on and a property test holds at every update.

## Phases

1. **The readings.** The million-note benchmark (`../architecture/testing.md`, Against SQLite) gains an `orderBy` on
   its indexed string with `limit 10`, a page of fifty near the order's end, a string `range`, and an order with no
   limit. Done when the four are recorded in `../evidence/million-notes.md` as the sorted arm answers them, with the
   size of the `d/x` of an index of strings nearly all different and of one of few values, and the time of a unit
   that writes each.
2. **The keys, the stamp and the order.** The three families in `format`'s table and the version raised; the keys
   filed through the one function by writes, `rebuild()` and the builder; the marker's rules, the stamp, the exact
   requirements and their `xtask` check; `orderBy`, `after`, `least` and `greatest` reading it. Done when:
   - the readings are recorded again, the first ten of a million reading ten entries where no entry is trashed, with
     both indexes' sizes and both units' times, and the decision above is made on them;
   - a property test holds the sign of the collator's comparison to the order of the keys as they are filed, cut and
     whole, and the cut to the whole key's first bytes, with no zero byte in any, over random strings in the
     collations of the third reading;
   - a model of the marker's rules runs every order of the units of a writer and a builder of each of two stamps, a
     `rebuild()` and a dropped index, and holds that no read trusts a build lacking a place; and a test shows it
     failing on each of: a builder that checks the build and not the stamp, a write that files under another
     stamp's marker, a build number used twice;
   - the model harness orders by an indexed string with a filter before the order, `after` both ways inside an entry
     of equal strings and inside a cut one, a compound index, the trash, a field of numbers and strings, a list of
     strings, strings past 256 bytes, a savepoint rolled back over a write that ended a build, and a read inside a
     unit;
   - `check()` reports a `d/xo` entry removed, one added and one moved, each by hand, and a list's string filed in
     `d/xo`; and a copy of the code in which each filing site in turn files `d/x` alone fails it;
   - a database filed under another collation with its stamp forged to this one's is shown ordered wrongly, which
     proves the test's keys differ; with its true stamp it orders from the rows until the builder ends and from the
     index after, as does a database whose collation changed, one copied by a backup in the middle of a build, and
     one a `rebuild()` of another stamp was run on while a builder held a build;
   - a field declared a string, then a `ref`, then a string again orders rightly at each;
   - an order on a number field reads its index under a marker of another stamp, and in a database opened
     read-only;
   - a watch on an ordered read is told when a commit files a string before the first it read, and when a write of
     another stamp ends the build it read.
3. **Ranges.** A string `range` reads `d/xo`, alone and before an order. Done when the model harness holds it to the
   rows for bounds on either side, inclusive and exclusive, a bound equal to the collator and of other bytes than a
   string held, and a bound whose key is cut; and the benchmark's range reads no entity outside its bounds but those
   of a cut entry.
