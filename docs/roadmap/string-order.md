# A string's order read from its index

Status: designed, 2026-11-14, and not built. It is the part of roadmap change 5 still open (`../roadmap.md`): an
`orderBy` on a string field reads every entity of the set, where one on a number reads ten entries for its first ten.
It adds derived keys and raises the format's version, so it is built before format 1 is frozen (change 8). This is
the fifth draft: each before it was read against the code by a second reader, the fourth was priced by a reading of
what its writes cost, and what each changed is under What was set aside.

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

The fourth, of what a write pays for an index, by `cargo run --release -p elmdb --example change-cost -- 1000000`
(Apple M1 Pro, 2026-11-14, built in release, two runs at loads of 24 to 25 and of 13.5 to 14.8; recorded in
`../evidence/million-notes.md`, A string's order, before an index reads it). A type of a million entities, each
unit setting one field of 1,000 of them spread over the type, CPU of the process for each entity, the median of five
units; and a create in types as large of a title and one string:

| What a unit does to each entity | First run | Second run |
|---|---:|---:|
| Sets a string no index reads | 14.7 µs | 14.4 µs |
| Sets a string one value index files | 32.2 µs | 33.2 µs |
| Sets a string two value indexes file | 45.7 µs | 49.1 µs |
| Sets an enum of two values, in one index | 33.8 µs | 36.8 µs |
| Sets a number, in one index | 31.5 µs | 37.4 µs |
| Creates it, its string in no index, in one, in two | | 14.0, 19.4 and 23.1 µs |

What they say: a sort key orders as the collator does; it has no useful bound, so a key is cut, and cut keys are met
in ordinary data; the cut is exact and cheap when the collator is stopped by its sink; and two strings of different
bytes can be equal to the collator, so a sort key can't stand in for a string where an equality is asked. And of the
writes: an index is about half of a change of its field, because the place is taken out of one leaf and put into
another that no other change of the unit writes; a second filing of a string adds 13.5 to 15.9 µs to its change,
42 to 48% of the unit, and 3.7 µs to a create; and an index of two values costs its change as much as one of strings
all different, 19 to 22 µs. Why was not read: by the layout its places are in blocks by place, and each change of a
unit spread over the type would write a block of its own under each value, which is an inference.

## The design

**Every value index a schema declares keeps the order of the strings its last field holds, a key for each distinct
string, and nothing is declared.** A caller who indexes a field and orders by it gets the index, without knowing
there was a choice. Whatever the field's declared type: what is filed doesn't change when a declaration does. The
engine's own indexes, of short codes and roles, are no schema's, are read for no order, and keep none.

**An order is a fact about an index's distinct strings; which entities hold each is in `d/x` already.** So the order
is a key for each string `d/x` has an entry for, holding nothing, and a read joins the two. A write that repeats a
string some entity holds files no key for the order. The derived families, each declared in `format`'s table:

| Key | Holds |
|---|---|
| `d/xo` type index build leading… sortkey cut string | nothing. It says the index's `d/x` has an entry for this string under what the index's fields before the last hold |
| `d/xo` type index build leading… sortkey cut place | nothing. It says the entity at this place holds a string that has no key of the kind above |
| `d/xob` type index | the order's marker: the stamp it was filed under, its build, and whether the build is whole |
| `d/xl` type index leading… | the places of the entities whose last field is an unplaced string (below), in blocks past 256 (`d/xlp`): among them the strings past 256 bytes, which `d/x` has no entry for and `d/xu` held until now |

- **A placed string** is one whose part in `d/x`'s key is no longer than a string of 256 bytes with no zero byte
  takes, 258 bytes. Its `d/xo` key holds the string as `d/x`'s key does, and is there exactly while its `d/x` entry
  holds a place: one key for a status a million entities hold, and a key each for a million names.
- **An unplaced string** is any other the field holds: one past 256 bytes, or one of fewer whose zero bytes, two
  bytes each in a key, make its part longer. Its entity has a `d/xo` key of its own, the entity's place where the
  string would be, and is in `d/xl`. So a string of any length is in the order, and what an unplaced string files
  follows from its entity alone. Its key holds a build and a sort key, so the function that derives an entity's
  keys hands it on apart from the entries and the rest, which are compared as sets of plain keys.
- **The sort key** is the collator's for the schema's collation at its default strength, a bytes part, made by a
  sink that keeps the bytes the key has room for and stops the collator at the next. The part after it says which:
  `false` for a whole key, `true` for one that was cut.
- **The room is one figure for an index and the values before its last field**: what LMDB's 511 bytes leave after
  the family's prefix, the type's and the index's names, the build, the leading parts, the cut's part and the 258
  bytes a placed string's part can take. By the parts' sizes as they stand that is some 230 bytes less the names
  and the leading parts, about 185 bytes of an ordinary title by the first reading's ratio; a test of pinned key
  bytes holds the figure in phase 2. It is one room and not one for each string, because two keys cut at different
  lengths can't be ordered by their bytes: a cut key is exactly the room long, and a whole one no longer. No `d/xo`
  key is longer than 511 bytes, whatever the names, the values and the strings.
- **Under 64 bytes of room the order of that tuple isn't kept**: no `d/xo` key is filed under it, of either kind,
  and a read of it reads the rows. A write and a read each work the room out from the names and the leading parts
  they hold, so nothing stored says it, and an order on the field's numbers is read from the index as now. Of an
  index whose names alone leave less, `indexStatus()` says its order isn't kept and that shorter names keep it.
- **What decides where a string is filed is the format's**: the 258 bytes, the room and the 64. `d/xl` is under no
  build, and two elmdbs that differed in any of the three would misfile each other's keys, so a change to one
  raises the format's version and not the stamp.
- **A list's strings have keys too.** `d/x` files each element of a list for an equality and the entity in `d/xu`,
  so no read reaches those keys while the list is there. They are filed all the same: the rule is every `d/x` entry
  of a placed string, with no exception to verify.
- **`build`** is the commit sequence of the unit that began the build, so no two builds of a database share one.
- **The format's version is raised with these keys**, so an elmdb from before them refuses the file where it would
  have written strings and filed no order.

**What keeps it.** Under a marker of the running stamp and a tuple with room, the `d/xo` keys holding a string are
those of `d/x`'s entries of placed strings, each under its sort key, and the keys holding a place are those of the
entities holding unplaced strings. Nothing but these writes the family:

- *The function that files a place under a `d/x` entry and takes one out* (`places::file`, `places::unfile`) is
  told by the list of places whether the entry came to be or went: `postings::put` finding no key of the entry's
  own, `postings::remove` deleting it. A list moved between the entry's own key and its blocks is neither. It puts
  the string's `d/xo` key at the first and deletes it at the second. Text terms, `d/xw` and `d/xu` are lists of
  the same kind and file no order, so it is this function's to do and not the list's.
- *The function that derives an entity's keys for an index* (`ValueIndex::files`) hands on, with `d/xl`, that the
  entity holds an unplaced string under a tuple, and a write, the builder, `rebuild()` and `check()` file its key
  where they build the others, with the type's name, under a tuple with room.
- *What clears or files `d/x` in bulk* does the same to `d/xo` in the same unit, deriving it from the `d/x` keys it
  files: `rebuild()` of one index and of all, which `renameType` and a database replaced by another's
  (`replace_with`) run; a dropped or changed index; `reset()`; and the unit that marks an empty type's index built.

**What reads it**, where the plan reads a number's entries today (one type, a built index, nothing in `d/xu`, and
for a compound index equalities on the fields before the last):

- An index that holds no string, neither in `d/x` nor in `d/xl`, is read as today and reads no marker: an order on
  numbers depends on no collator.
- One that holds strings is read when its order's marker is this process's and whole, and the tuple has room:
  false, true and the numbers from `d/x`, the entities without a value from `d/xw`, and the strings by a walk of
  `d/xo` under the marker's build, backwards when descending. For a key holding a string, a seek to that string's
  `d/x` entry hands on its places in their order; a key holding a place hands on that entity. Otherwise the rows
  are read, as today.
- A run is the keys of one sort key and one cut. One whose cut is `false` and whose keys each hold a string is
  handed on as it is walked: its strings are equal to the collator, and their keys are in the order of their
  bytes. Any other run, of cut keys or with a key holding a place, is held whole and ordered before any of it is
  handed on: by the collator's comparison of the strings, then their bytes, then the place, each string from its
  key, or from its entity's row for a key holding a place. It is held whole because the keys' own order within it
  says nothing: a key holding a place is before every key holding a string, whatever their strings. The collator compares them and no whole sort key is made, an unplaced string having no bound. A row is
  read only for an unplaced string in a run the walk reaches.
- `after`, a page's cursor, is an entity: its string is read from its row, and the walk begins at that string's key
  and the entity's place in its entry, or at the entity's own key when the string is unplaced. In a run that is
  ordered first, the run is ordered again for each page, and the page goes on past the cursor in that order.
- `least` and `greatest`: the first entity the order hands on within the scope, and its string from its key, or
  from its row when the key holds a place.
- `range` with string bounds (phase 3): a seek to the lower bound's sort key and a walk to the upper's, the keys
  whose cut sort key is a bound's tested by the collator on their strings.
- A count is the places of each entry the walk passes, counted as an entry's are today.
- The marker, the keys walked and the entries read are recorded reads, so a watch on an ordered read is run again
  when a string comes to be before the first it read, when an entity takes a string it read, and when the order's
  build changes.

An equality and `unique` read `d/x` as now, and depend on no sort key.

**Strings the collator holds equal are ordered by their bytes.** The order is the collation's, then the strings'
bytes, then as ties are ordered today: what a walk of the keys gives, with no merge by place across strings. The
sorted arm compares the same way in each of its three places (the sort, the first few kept in a pass, `least` and
`greatest`), so the two arms agree and the order is total. Two entities tie only when they hold the same bytes.
There are then three things a query can ask of two strings, and the documents say which is which: an equality is of
bytes, a `range`'s bounds are the collator's alone, and an order is the collator's and then the bytes'.

**A stale sort key is never read.** A sort key is the collator's output, and ICU's documentation says to presume
stored ones invalid after an update of CLDR, of Unicode or of the library. Read stale, they give a wrong order with no
error. The rules, each unit reading the marker in its own transaction and through the unit, so a savepoint rolled
back restores it:

- *A read* uses `d/xo` under the marker's build only when the marker's stamp is its own and the build is whole.
- *A write* reads the marker when a placed string gains its first holder or loses its last under a tuple, and
  whenever an entity holding an unplaced string is filed under a tuple or taken out from under one, or its string
  changes, whether or not a key of its own stamp would change: two unplaced strings of one cut sort key to this
  collator may be of two to another, and a change to a field before the last moves the entity's key to another
  tuple with its string as it was. Under a tuple without room no stamp files a key, and a write reads no marker. It files under the marker's build when the
  marker's stamp is its own, whole or not. Under a marker of another stamp it deletes the marker, once, and files
  nothing; with no marker it files nothing. So no build is whole that lacks a key, whoever wrote. A write that
  gives an entity a placed string another holds reads no marker and ends no build: which strings have keys depends
  on no collator, and the entity is found through the string's `d/x` entry.
- *An index's order begins whole.* The unit that declares an index, whose `d/x` is empty, writes the marker with
  its stamp, its sequence as the build, and whole: an empty `d/x` has an empty order. What fills `d/x` after it,
  the value index's builder and the writes beside it, files the order by the functions above, so the two are whole
  together. *`rebuild()`* files `d/x` and the order whole under a new build, in its unit.
- *The order's builder* is for an order that isn't this stamp's and whole: one another stamp ended, or one filed
  before an update of the collator. It goes on with a build of its own stamp that isn't whole, and otherwise begins
  one: a marker of its stamp, its unit's sequence, not whole. It walks the index's `d/x` keys, not the rows, and
  the rows of the entities in `d/xl`, in units that go on from the last key done, putting each key that isn't
  there, beside the writes of its stamp that keep the keys on both sides of where it is. A build it finds
  unfinished it goes on with from the index's first key, a key already there being read and not written. Each of
  its units checks that the marker is still its stamp and its build, not whole, and stops if not; the last marks
  it whole. An order another stamp ended or replaced is tried again after a wait that doubles, so two elmdb
  versions that hold one database open leave its orders unbuilt, and read from rows, more of the time the longer
  both stay.
- *What tells the builder*: an open that can write, for every declared index whose marker is not its own and whole;
  a unit that deleted a marker or found none where it read for one, once it commits; and, in a process that can
  write, an ordered read that found the marker not its own and whole, so a process whose writes only repeat
  strings has its orders built again too.
- *A dropped index* has its `d/xo`, `d/xl` and marker cleared in the unit that clears its `d/x`.
- *Keys under a build that is not the marker's* are no order's. The builder deletes them in units of their own, each
  reading the marker again; they sort before the marker's build, so finding none is one seek.
- *The schema's collation* is in the stamp, so a change's effect becomes `Builds` where it is `Applied` today, by the
  same path.

**The stamp** covers what can change a sort key's bytes and nothing else. It is a SHA-256 of: the schema's
collation; a number for the collator's options and the sink, raised with any change to the function that makes a
sort key and held by a test of pinned key bytes; and the name and version of every ICU crate in the collator's
dependency tree. Each of those crates is a direct dependency
required exactly (`=`), which `cargo xtask` holds to the lockfile and to the stamp's list, so a consumer of the Rust
crate resolves the same versions or fails to resolve. The build refuses `ICU4X_DATA_DIR`. A `[patch]` or a vendored
copy can still change what a version means; for that, the stamp also takes the sort keys of a few dozen probe strings
made by the running collator, in the schema's collation, when a database is opened. The probes are a sample and a
backstop, not the proof. The collator is made once for a collation in a process and kept.

**`check()`** derives, under a whole build of its own stamp, the `d/xo` keys from the `d/x` keys it has derived
from the rows and from the entities holding unplaced strings, and `d/xl`, and reports a key missing, one too many
and one under the wrong sort key, each by name. Under another stamp it compares the strings and the places the keys
hold, says the sort keys were filed by another collator and were not compared, which is what `elmdb-cli` of another
build meets. `indexStatus()` and the wait for index builds cover an order as they cover an index.

**What it costs**, by the fourth reading. A write that repeats a string another entity holds: nothing, where a
second place for every entity would by that reading's enum have paid some 19 to 22 µs again for a status, an
estimate, since no reading filed an enum twice. A write that gives an entity a string no other holds: the marker
read, one sort key and one key put, and one deleted when the string it had goes with it. A title's sort key is
775 ns by the first reading; a long string's is cut as it is made, 21 µs for 10,000 bytes by the third, though a
string of characters the collation ignores is read to its end. The key is a leaf written and another when one is
deleted, as a second index of the string is, which cost 13.5 to 15.9 µs on a change of 32.2 to 33.2 at a million
entities and 3.7 µs on a create; this key is longer than that index's, holding the sort key and the string, and
what that adds is read in phase 2. An index grows by a key for each distinct string, holding its sort key and the
string: for names nearly all different, of a dozen bytes, something under twice its `d/x` by the parts' sizes,
which is an estimate and no reading; for a field of few values, nothing to speak of. An update of the collator's
crates builds every index's order again, once, in the background, from the index's keys. A read pays one seek for
each distinct string it hands on.

**The decisions the readings make**, in phase 2:

- *The seek.* Every note of a million in the names' order is read from the index and its time recorded beside the
  sorted arm's (2,534 ms in the reading of 2026-11-13, `../evidence/million-notes.md`). If the seeks to `d/x` are
  more than a third of it, a string one entity holds keeps that entity's place as its key's value: written with
  the key, replaced by nothing when a second entity takes the string, and written again when the second-last
  leaves. A read then seeks only for a string several hold. Those are two more writes that read the marker, and a
  writer without them would take both for repeats: it is the format's, and is in or out before the freeze.
- *The writes.* The fourth reading's program is run with the order and without, in one run, the two runs above
  differing by as much as a fifth between them. The unit that changes a thousand statuses costs no more than a
  tenth over what it costs without the order, and what the order adds to the unit that changes a thousand names is
  no more than half as much again as what a second index adds. Past either, something files that this design says
  doesn't, and it is found before anything else is built.
- *The file.* More than a tenth larger at a million notes, and the keys are made smaller before the freeze.

## What was set aside

- **An index that asks for it (`"collated": true`).** The first draft. A caller who doesn't know to ask gets the
  rows read, silently, and the option is a path most indexes and most tests never run.
- **A directory of distinct strings as the second draft had it.** It made each string's key unique by a hash, merged
  by place the strings the collator holds equal, and held unbounded values for cut keys, and the reader of the
  third draft held it rightly set aside. This draft is that directory without the three: the string is in the key,
  equal strings are ordered by their bytes, and a key holds nothing. What stands of the objections is paid: a key
  written where a `d/x` entry first appears and last goes, and a second read for each string.
- **A second place for every entity**, `d/x` again with a sort key where the string was. The fourth draft, and
  simpler: one function derives every family from an entity alone. The fourth reading priced it at 42 to 48% of a
  change of an indexed string against its own limit of a tenth, and by the enum's reading it would cost a change
  of a status about as much again, which nobody orders by and which is among the commonest writes a program makes.
- **A field type for identifiers, ordered by its bytes from `d/x`**, which the fourth draft's decision named, and a
  collation of bytes declared for a field. Another session drafted the first on 2026-11-14 and a reader found seven
  defects in it against the code; the reader of this draft found the same of the second. The sorted arm orders a
  set of several types with one comparison, so a field ordered by bytes in one type and by the collation in
  another has no total order; a `range` would go on comparing by the collation and disagree with the order; and a
  narrowed field is enforced and not refused, so rows past the type's bound would stand. What it would save is
  small: an identifier is set when its entity is made, where the order's key costs 3.7 µs.
- **An index that asks for no order.** It adds no key and can't give a wrong order, so it can come after the
  freeze if a program's readings ask for it.
- **The order's changes kept in a journal the builder folds in.** The one arrangement that costs a write of a new
  string less in its own unit. An order would then lag its commit, and it is more than can be verified before the
  freeze.
- **The order built when a plan first wants it.** It costs nothing for an index never ordered, and the second reader
  held it the one rival worth weighing again at the decision. Against it: what is stored would depend on which
  plans ran and not on the rows and the schema, a database opened read-only could never have it, and a caller's
  tests would run one arm where production runs the other. With a repeated string free, what it would save is the
  key of a string that is never ordered and never repeated.
- **An order kept in memory by each process**, as the edge mirror keeps edges. No stored key to go stale; some second
  and tens of megabytes for a million strings at every start, emptied by another process's commit.
- **`d/x` itself keyed by the sort key, then the string.** One family, exact equality kept, and the only arrangement
  in which a new string writes one ordered structure and not two. But every equality and `unique` would then wait
  on a build after a collator update.
- **The order's keys put by the list of places itself.** Text terms, `d/xw`, `d/xu` and the engine's own indexes
  are lists of the same kind and have no order.
- **A room for each string**, what its own part leaves. A walk can't tell whether a key belongs to the run of a key
  cut shorter.
- **The longest part `d/x` can hold taken out of every room.** `d/x` holds a string of 256 bytes whatever its zero
  bytes make of its part, as far as its key fits, and that part leaves a sort key no room at all.
- **An order built only once its `d/x` is.** The functions that fill `d/x` file the order as they go, so an index's
  order is whole from the unit that leaves its `d/x` empty.
- **The unplaced strings read from their rows and merged into every walk.** A page of ten would read the row of
  every long string the index holds.
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
- **A tuple with too little room filed in `d/xu`**, as the fourth draft had it. `d/xu` also says whether an order
  and a range on the field's numbers can be read from the index, which a long name has nothing to do with, and
  the function that derives an entity's keys isn't given the type's name to work a room out from.
- **Building in place**, by clearing the old keys and filing the new. A writer of the same stamp, a second builder
  and a writer of another stamp each meet a range half cleared; a build under a new sequence meets none.
- **Ordering by bytes and calling that the order.** It is what the index has, and it is wrong for any text a person
  reads: capitals before every lowercase letter, accented letters after `z`.

## Risks

- **The two arms must agree.** The sorted arm stays, for sets no index orders, and is what the model harness holds
  the index's order to. The harness's strings are made of `a`, `b` and `c` today and its model sorts by the
  collator alone: it can't see a tie of other bytes or a key past the room until phase 2 gives it both.
- **An invariant no one entity derives.** A `d/xo` key is there because of every entity holding its string, so a
  site that writes `d/x` and not `d/xo` loses an entity from an order with no error. The sites are named above and
  each is a gate below; the harness has six entities, so none of its entries is ever in blocks, and the first and
  last place of an entry in blocks is tested apart.
- **The seek for each string**, on a read that hands on every entity. The first decision above answers it.
- **Half the fourth draft's room**, the string being in the key: more runs are cut. A cut run of placed strings is
  ordered from its keys, and reads a row only for an unplaced string.
- **A change of an indexed string no other entity holds** costs what a second index would. By the readings the
  benchmark's unit that renames a thousand notes would then cost about what SQLite's does with its one index
  (35.3 ms, against elmdb's 22.6 and the 13.5 to 15.9 the key adds), an estimate until phase 2 reads it. No
  arrangement pays less that keeps an equality independent of the collator and an order exact at its commit.
- **A build at every update of the collator's crates**, for every database, at its next open that can write.
- **The sink is the crate's `unstable` interface.** It is exempt from the crate's versioning, which the exact
  requirements make safe to depend on and a property test holds at every update.

## Phases

1. **The readings.** Taken on 2026-11-13 and 2026-11-14 and recorded in `../evidence/million-notes.md` (A string's
   order, before an index reads it): the four reads of the names' order as the sorted arm answers them, each
   index's bytes, the time of a unit that changes each, and the fourth reading above.
2. **The keys, the stamp and the order.** The families in `format`'s table and the version raised; the keys
   filed by the two functions and at each bulk site; the marker's rules, the stamp, the exact requirements and their
   `xtask` check; the tie by bytes in both arms; `orderBy`, `after`, `least` and `greatest` reading it. Done when:
   - the readings are recorded again, the first ten of a million reading ten keys and ten entries where no entry is
     trashed, with both indexes' sizes and both units' times, and the three decisions above are made on them;
   - a property test holds the sign of the collator's comparison, then the bytes', to the order of the keys as they
     are filed, cut and whole, and the cut to the whole key's first bytes and to exactly the room's length, with no
     zero byte in any sort key, over random strings in the collations of the third reading; and holds every key to
     511 bytes over random names, leading values and strings, zero bytes among them;
   - a test of the list of places holds that an entry comes to be at its first place and goes at its last, and at
     no other, through an entry that passes into blocks and back; and a copy of the code that says so when a list
     moves from blocks to the entry's own key, and one that misses the last place, each fails it. With it, apart
     from the harness, whose six entities fill no block: a string more than 256 entities hold is ordered, paged
     and passed by `check()` as entities take it and leave it;
   - a model of the marker's rules runs every order of the units of a writer and a builder of each of two stamps, a
     `rebuild()` and a dropped index, and holds that no read trusts a build lacking a key; and a test shows it
     failing on each of: a builder that checks the build and not the stamp, a write that files under another
     stamp's marker, a build number used twice;
   - the model harness, its model ordering by the collation and then the bytes, orders by an indexed string with a
     filter before the order, `after` both ways inside an entry of one string, among strings the collator holds
     equal and inside a cut run, from a cursor holding an unplaced string, a compound index, the trash, a field of
     numbers and strings, a list of strings, strings past 256 bytes and strings of zero bytes, a savepoint rolled
     back over a write that ended a build, and a read inside a unit;
   - `check()` reports a `d/xo` key removed, one added and one moved, each by hand; and a copy of the code in which
     each of these in turn writes `d/x` alone fails it: a write, the builder, `rebuild()` of one index and of all,
     `renameType`, a database replaced and a changed index. `check()` can't see the other three, so each has its
     own: after a dropped index no key is left under its `d/xo`, `d/xob`, `d/xl` and `d/xlp`; and after `reset()`,
     and after an index is declared on an empty type, entities are made with strings and ordered before the
     builder can run, in the unit that made them, and the read visits as many entities as its limit, which a copy
     that writes no marker there fails;
   - a database filed under another collation with its stamp forged to this one's is shown ordered wrongly, which
     proves the test's keys differ; with its true stamp it orders from the rows until the builder ends and from the
     index after, as does a database whose collation changed, one copied by a backup in the middle of a build, and
     one a `rebuild()` of another stamp was run on while a builder held a build;
   - a field declared a string, then a `ref`, then a string again orders rightly at each;
   - an order on a number field reads its index under a marker of another stamp, and in a database opened
     read-only;
   - a watch on an ordered read is told when a commit files a string before the first it read, when an entity
     takes a string it read, and when a write of another stamp ends the build it read;
   - a write of another stamp ends the build when it changes an unplaced string to one of the same cut sort key,
     and when it changes a field before the last of a compound index for an entity holding an unplaced string; one
     that gives an entity a placed string another holds doesn't, the order read after it being right; and a
     process whose writes only repeat strings has an order another stamp ended built again after an ordered read.
3. **Ranges.** A string `range` reads `d/xo`, alone and before an order. Done when the model harness holds it to the
   rows for bounds on either side, inclusive and exclusive, a bound equal to the collator and of other bytes than a
   string held, and a bound whose key is cut; and the benchmark's range reads no entity outside its bounds but those
   of an unplaced string in a run at a bound.
