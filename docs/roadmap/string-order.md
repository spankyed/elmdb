# A string's order read from its index

Status: designed, 2026-11-13, and not built. It is the part of roadmap change 5 still open (`../roadmap.md`): an
`orderBy` on a string field sorts every entity of the set, where one on a number reads ten entries for its first ten.
It adds derived keys, so it is decided before format 1 is frozen (change 8).

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

**An index declares that it is collated.** A value index takes `"collated": true`. It then also files its last
field's strings in the collation's order, in keys of their own, and leaves its `d/x` entries as they are. It is opt-in
because it costs space and write time that an index read only by equality shouldn't pay.

```json
"byTitle": { "kind": "value", "fields": ["title"], "collated": true }
```

**The keys.** One more derived prefix, declared in `format`'s table:

| Key | Holds |
|---|---|
| `d/xo` type index leading… sortkey | the places of the entities whose last field holds a string with this sort key, under what the index's fields before the last hold: as `d/x` holds a value's places, in blocks past 256 (`d/xop`) |

- The sort key is the collator's for the schema's collation at its default strength, written as the key's last part.
  It holds no zero byte, so it is filed as a string part is.
- Strings the collator holds equal share a key, and their entities are in place order under it. That is the order the
  sorted arm gives today, a stable sort of place order, so the two arms answer the same and the model harness can hold
  one to the other.
- A string the index doesn't file under `d/x` (past `MAX_INDEXED_STRING`) is in `d/xu` already, and stays there. A
  filed string's sort key is 389 bytes at most by the reading above, and with the type's and index's names may pass
  LMDB's 511-byte key. Such a string is filed in `d/x` and not in `d/xo`, and the index counts it: an index with any
  is not read for an order, as one holding a list isn't today.
- `d/xw`, the entities without a value, is shared: an order reads it last, or first when descending, as a number's
  does.

**What reads it.** Each where the plan would have read a number's entries, under the same conditions (one type, a
built index, nothing in `d/xu`, and for a compound index equalities on the fields before the last):

- `orderBy` on the field: `d/xo` walked forward or back, each entity handed on as its entry is read. The first ten of
  a million read ten entries.
- `after`, a page's cursor: a seek to the cursor's sort key and place, so a page costs the same wherever it is.
- `range` with string bounds: a seek to the lower bound's sort key and a walk to the upper's.
- `least` and `greatest`: the first entry from either end, and the value read from that entity's row, since the key
  holds the sort key and not the string.

An equality, `unique`, and an order on booleans and numbers read `d/x` as now. An index not declared collated sorts
as now.

**What invalidates it.** A sort key is the collator's output, which three things change:

- *The schema's collation.* The change's effect becomes `Builds` for each collated index, where it is `Applied`
  today: the builder files `d/xo` again, and until it has, plans sort.
- *The collator's data.* ICU's documentation says to presume stored sort keys invalid after an update of CLDR, of
  Unicode or of the library. elmdb holds a constant, `SORT_KEY_VERSION`, and a test that pins the sort keys of a fixed
  set of strings in each collation the tests use: an update that changes any of them fails the test, and the fix is to
  raise the constant. Each collated index's built marker (`d/xb`) records the collation and the constant it was filed
  under; an open that finds another queues the index for the builder, as a schema change would.
- *A file moved between elmdb versions.* The same marker covers it: an older or newer elmdb with another constant
  rebuilds `d/xo` before it reads it. The keys are derived, so nothing is lost, and `check()` holds `d/xo` to what the
  rows and the running collator derive.

**What it costs.** For each string filed in a collated index: one sort key made at the commit, 775 ns by the reading
above, and one more entry, of the sort key's length where `d/x`'s is the string's. A rebuild makes every key again.
The space is expected to be about what the index's `d/x` entries take, and is read in phase 1 before anything is
built.

## What was set aside

- **Replacing `d/x`'s string entries with sort keys.** One family instead of two, but an equality would then find
  every string the collator holds equal to the one asked for and need each row read to tell them apart, `unique`
  would refuse `é` in two forms, and a collator update would take equality lookups away until the rebuild ends.
- **Filing the string after its sort key in one key**, which keeps equality exact. A sort key and its string together
  pass LMDB's key size from about 200 bytes of string, so the index would file fewer strings than it does today.
- **Every string index collated, with no declaration.** It doubles each such index's entries for callers who never
  order by the field.
- **Ordering by bytes and calling that the order.** It is what the index has, and it is wrong for any text a person
  reads: capitals before every lowercase letter, accented letters after `z`.

## Risks

- **A collator update the pinned strings don't show.** The test's strings are a sample. The set covers each script
  and each tailored locale the tests open, and `check()` on any database finds a `d/xo` that the running collator
  doesn't derive; a release's notes name a raised constant.
- **A rebuild at every dependency update that touches collation.** At a million strings the keys alone are under a
  second by the reading above; the builder's writes are the larger part and are read in phase 2.
- **Two arms that must agree.** The sorted arm stays, for sets no index orders. The model harness already holds an
  order read from an index to the sorted one for numbers, and takes strings into the same step.

## Phases

1. **The readings.** The million-note benchmark (`../architecture/testing.md`, Against SQLite) gains an `orderBy` on
   its indexed string with `limit 10`, a page of fifty near the order's end, and a string `range`. Done when all three
   are recorded in `../evidence/million-notes.md` as the sorted arm answers them, with the `d/x` size of that index.
2. **The keys and the order.** `collated`, `d/xo` and `d/xop` in `format`'s table, filed by writes, `rebuild()` and
   the builder, and `orderBy`, `after`, `least` and `greatest` reading them. Done when: the first ten of a million read
   ten entities and a page costs the same wherever its cursor is, both recorded beside phase 1's; the model harness
   orders by a string field in a collated index and in one that isn't; `check()` reports a `d/xo` entry changed by
   hand; the index's size and a unit's time are recorded before and after.
3. **Ranges.** A string `range` reads `d/xo`. Done when the benchmark's range reads no entity outside its bounds.
4. **Invalidation.** `SORT_KEY_VERSION`, the pinned keys' test, the marker in `d/xb`, and `Builds` for a collation
   change. Done when: a database filed under another constant, and one whose collation changed, each order by sorting
   until the builder ends and by the index after; a test opens both; the pinned test is shown failing on a changed
   key.

A smaller change needs none of this and helps every order no index reads: the sorted arm keeps its entities in a heap
and hands them on as they are popped, so a `limit 10` after it costs one pass and ten pops where it costs a whole sort.
By the reading above that is 335 ms against 2,351 ms for a million, before the fields are read. It changes no key and
can be built first.
