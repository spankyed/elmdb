# A string's order read from its index

Status: designed, 2026-11-13, and not built. It is the part of roadmap change 5 still open (`../roadmap.md`): an
`orderBy` on a string field sorts every entity of the set, where one on a number reads ten entries for its first ten.
It adds derived keys, so it is built before format 1 is frozen (change 8).

## Where it stands

A value index files a string under its own bytes (`../architecture/format.md`, `d/x`), which is what an equality and
`unique` need. An `orderBy` and a `range` with string bounds order strings by the schema's collation, ICU's root or a
locale's (`../architecture/schema.md`, `collation`), and the collation's order is not the bytes' order: `a` sorts
before `B`, and `é` beside `e`. So an order on a string field reads the field of every entity in the set, sorts them
with the collator, and hands them on (`engine/reads.rs`, the `orderBy` step's sorted arm), and a string `range` tests
each entity it is given. A `limit 10` after the order saves nothing: the sort is whole before the first is handed on.

A change of the schema's collation is applied at once and costs nothing, because no stored key depends on it
(`../architecture/schema.md`, Changing a schema).

## The measurements

One run of a disposable program on a copy of `main` at `0933e43` (Apple M1 Pro, 2026-11-13, load 14.4, built in
release, `icu_collator` 2.4.0, root collation): a million titles of three to six words from a vocabulary of 4,000, a
tenth of the words with an accented letter and a quarter capitalized, 33.6 bytes a title. No database is in it: these
are the collator's costs alone, and the sorted arm also reads each entity's field.

| What | Reading |
|---|---|
| A sort key | 40.2 bytes a title, 1.2 times the string; 775 ns to make |
| The million sorted by the collator's comparison | 2,351 ms |
| The million sorted by their sort keys' bytes | 211 ms, the same order |
| The ten least kept in one pass of comparisons, no sort | 335 ms |
| A sort key's longest, of a string the index files (256 bytes at most) | 389 bytes, for 128 `é`; 269 for 256 ASCII letters |
| A zero byte in any of the million keys | none |
| `é` composed and decomposed | equal to the collator, and one sort key |
| `a` and `A` | different sort keys |

What they say: the comparison sort is the cost an index would remove, some 2.4 µs an entity before any field is read;
a sort key orders as the collator does and is a little longer than its string; and two strings of different bytes can
be equal to the collator, so a sort key can't stand in for the string where an equality is asked.

## The design

**Every value index orders its strings, and nothing is declared.** A caller who indexes a field and orders by it gets
the index's speed without knowing there was a choice to make, and one code path is the one every index, test and
model step runs. What makes that affordable is the next point: the cost is by distinct string, not by entity.

**The keys are a directory of the index's strings, in the collation's order.** One more derived prefix, declared in
`format`'s table:

| Key | Holds |
|---|---|
| `d/xo` type index leading… sortkey | the strings the index files under these leading values whose sort key this is: one, or the few the collator holds equal or that share the key's bytes |

- It holds no places. A string's entities are where they are today, under the string in `d/x`: an order reads a
  `d/xo` entry for the next string and that string's `d/x` entry for its entities. So a field of ten values over a
  million entities adds ten entries, a write that gives an entity a string the index already holds makes no sort key
  and writes nothing here, and a rebuild reads the index's own keys and no row.
- An entry is written when a string's `d/x` key is first written and deleted when that key is, in the same unit.
- The sort key is the collator's for the schema's collation at its default strength, filed as a bytes part. It is cut
  to the room the key has under LMDB's 511 bytes, the same room for every string under the same leading values, so a
  key's bytes order as the whole sort keys do wherever they differ within it. Strings that share a key are ordered by
  the collator as they are read. No string the index files is left out, so one long title can't take the index away
  from every order on its field.
- Strings the collator holds equal are read together, their entities merged in place order. That is the order the
  sorted arm gives today, a stable sort of place order, so the two arms answer the same.
- `d/xw`, the entities without a value, is read last, or first when descending, as a number's order reads it.
  A string past `MAX_INDEXED_STRING`, a list, and the other values `d/xu` holds stay unordered, and an index holding
  any sorts as it does today, which EXPLAIN says with the reason.

**What reads it.** Each where the plan would have read a number's entries, under the same conditions (one type, a
built index, nothing in `d/xu`, and for a compound index equalities on the fields before the last):

- `orderBy` on the field: `d/xo` walked forward or back, each string's entities handed on as its `d/x` entry is read.
  The first ten of a million read ten strings at most.
- `after`, a page's cursor: a seek to the cursor's sort key, string and place, so a page costs the same wherever it
  is.
- `range` with string bounds: a seek to the lower bound's sort key and a walk to the upper's.
- `least` and `greatest`: the first entry from either end, which holds the string itself.

An equality, `unique`, and an order on booleans and numbers read `d/x` as now.

**A stale sort key is never read.** A sort key is the collator's output, and ICU's documentation says to presume
stored ones invalid after an update of CLDR, of Unicode or of the library. Read stale, they give a wrong order with no
error, which is the failure this design is built around:

- *The stamp.* The collator's crates are pinned to exact versions in `Cargo.toml`, and elmdb holds those versions in
  a constant that `cargo xtask` holds to the lockfile, so the two can't part. An index's built marker (`d/xb`) records
  the collation and that constant as its strings were filed.
- *Every transaction checks it.* A plan reads the marker in its own snapshot before it reads `d/xo`, and sorts when
  the marker names another collation or another constant. A write that finds one clears the marker's record of the
  order, so no reader of any process trusts `d/xo`, and queues it for the builder, which files it again from `d/x`.
  Two processes of different elmdb versions on one database each rebuild after the other; neither reads the other's
  keys.
- *The schema's collation.* A change's effect becomes `Builds` where it is `Applied` today, by the same path.
- *The proof.* `check()` derives `d/xo` from `d/x` with the running collator and reports any difference. A test pins
  the sort keys of a fixed set of strings in each collation the tests open, so an update that changes one is seen in
  review, not only by the constant.

**What it costs.** For each distinct string an index files: one sort key, 775 ns by the reading above, and one entry
holding the sort key and the string. Nothing for a write that repeats a string. An index whose strings are nearly all
different, a title or an id, about doubles in size by that arithmetic; the reading is phase 1's, and no option to
leave an index unordered is built unless it shows a cost worth one (below).

## What was set aside

- **An index that asks for it (`"collated": true`).** The first draft of this design. A caller who doesn't know to
  ask gets the sort, silently, and the option is a path most indexes and most tests never run. It comes back, as a
  way to opt out, only on a reading: an index never ordered whose `d/xo` is over a tenth of a compacted file.
- **The places filed a second time under the sort key.** No second read for a string's entities, but every write of
  the field makes a sort key and writes two entries, a field of few values over many entities doubles, a rebuild
  reads every row, and telling apart strings that share a key means reading rows.
- **Replacing `d/x`'s string entries with sort keys.** One family instead of two, but an equality would then find
  every string the collator holds equal to the one asked for, `unique` would refuse `é` in two forms, and a collator
  update would take equality lookups away until the rebuild ends.
- **Leaving out a string whose sort key is too long.** One such string would send every order on the field back to
  sorting.
- **A constant raised by hand when a test of pinned sort keys fails.** The test samples; the pinned versions don't.
- **Ordering by bytes and calling that the order.** It is what the index has, and it is wrong for any text a person
  reads: capitals before every lowercase letter, accented letters after `z`.

## Risks

- **The two arms must agree.** The sorted arm stays, for sets no index orders. The model harness already holds an
  order read from an index to the sorted one for numbers, and takes strings into the same step, with strings the
  collator holds equal and strings long enough to share a cut key among its values.
- **A rebuild at every update of the collator's crates.** It reads `d/x`'s keys, not rows: under a second of sort
  keys for a million distinct strings by the reading above, and the builder's writes, read in phase 2. Until it ends
  plans sort, as they do today.
- **A second read for each string.** An order over strings that are nearly all different reads two entries an
  entity where a number's reads one. Phase 2's gate reads it against the sort it replaces.

## Phases

1. **The sorted arm hands on from a heap.** It changes no key and helps every order no index reads: a `limit 10`
   costs one pass and ten pops where it costs a whole sort, 335 ms against 2,351 ms for a million by the reading
   above, before the fields are read. Done when a test counts the comparisons of an order with a limit, and the
   million-note benchmark (`../architecture/testing.md`, Against SQLite) records an `orderBy` on its indexed string
   with `limit 10`, a page of fifty near the order's end, and a string `range`, before and after, with that index's
   `d/x` size.
2. **The keys and the order.** `d/xo` in `format`'s table, filed by writes, `rebuild()` and the builder, with the
   stamp in `d/xb`, the pinned versions and their `xtask` check from the start; `orderBy`, `after`, `least` and
   `greatest` reading it. Done when: the first ten of a million read ten strings and a page costs the same wherever
   its cursor is, both recorded beside phase 1's; the model harness orders by an indexed string; `check()` reports a
   `d/xo` entry changed by hand; a database filed under another constant, and one whose collation changed, each
   order by sorting until the builder ends and by the index after, and a test opens both; the index's size and a
   unit's time are recorded before and after.
3. **Ranges.** A string `range` reads `d/xo`. Done when the benchmark's range reads no string outside its bounds.
