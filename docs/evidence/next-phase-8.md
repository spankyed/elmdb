# Phase 8 of goal-elmdb-next: value compression

2026-10-27, elmdb `933aa19` (apack pins it at `6d14be5b24`). Format 1 changed in place (G3): `docs/architecture/format.md`,
Values. apack's `docs/archive/plans/ears-value-compression.md`, written for the engine before elmdb, is superseded by it.

## What is stored

A field's value of `COMPRESS_FROM` (256) bytes of msgpack or more is stored as the byte `0xc1`, which no msgpack value
starts with, and a zstd frame (level 3) of its msgpack, when that is smaller (`values::stored`); every other value is its
msgpack as given. Readers decompress where they read a field (`values::unstored`, `decode_stored`): a row and a field
answer, a filter, a value index's and a text and vector index's derivation, `check()`, `rebuild()`, a dump's export.
An admin rename copies the stored form, and the undo log restores it. The `zstd` crate is new, a native dependency on
every target: PR #3's eight-target CI passed on `933aa19`.

Specs (`crates/elmdb/src/engine/tests/compression.rs`): a long message is stored tagged and under a quarter of its
msgpack; a short one is its msgpack byte for byte; random bytes, which compressing would enlarge, are kept as given;
and a row, a field, a filter on a value past what an index holds, a text search, `check()` before and after `rebuild()`
and a dump all see the value written. Two disposable mutations on a copy-on-write clone fail them: a row read that splices
the stored bytes unchanged, and a codec that never compresses. `lifecycle.rs`'s map-full case wrote 4 MiB of `x`, which
now compresses to almost nothing; it writes text that doesn't compress, being about a value too large for the map.

## The threshold, on production-shaped data

G8's dump loaded by the `elmdb` CLI built at each threshold, in a copy-on-write clone, then compacted (`elmdb backup`)
and measured; and, at three of them, every Message and every entity read as rows through the binding built alike,
read-only and warm, medians of seven (2026-10-27, Apple M1 Pro, load 7 to 11):

| `COMPRESS_FROM` | Compacted file | Every Message's row (19,681) | Every entity's row (22,694) |
|---|---:|---:|---:|
| none (as stored before) | 267.8 MB | 394.7 ms | 428.2 ms |
| 256 | 122.1 MB | 535.0 ms | 577.4 ms |
| 512 | 136.6 MB | 525.6 ms | 573.1 ms |
| 1,024 | 131.3 MB | | |
| 2,048 | 139.0 MB | | |

256 is the smallest file, and reads at 256 and 512 cost the same, about 7 µs to decompress each message read whole: a
whole read of every message costs a third more, for less than half the file. apack's views read a thread's page, not
every message, and the readings are warm, so a cold read's fewer pages aren't credited. 1,024 below 512 is LMDB's page
layout, a value past about 2 KB going to overflow pages of its own, so which values fit in a leaf once compressed moves
the total in steps.

## Our databases

None needs converting: a value written before compression is untagged, which reads exactly as it did, and a write from
now on stores the new form. G8's dump holds values, not stored forms, so it needs nothing either. The copy loaded from
it at `f7bc8a5` (`~/Develop/elmdb-next-scratch/prod`) reads as it is.

## G4

apack's `npm run bench -w @apackdev/ears`, the engine over elmdb `c3c8021` (before, built in a copy-on-write clone) and
`933aa19` (after), alternated twice at a load of 8 to 17; the lower of each case's two medians, in ms. The bench's values
are short, below the threshold, so it reads the codec's check of a value's first byte, not compression:

| Case | Before | After | |
|---|---:|---:|---:|
| bulk load 50k entities, 200k attributes, 100k relations | 1346.90 | 1323.40 | −1.7% |
| query by type with where and pickAll | 36.65 | 37.06 | +1.1% |
| 1,000 where lookups by a unique value | 7.42 | 6.68 | −9.9% |
| 10 where lookups on an unindexed field | 249.39 | 255.80 | +2.6% |
| 1,000 getRow | 5.41 | 4.98 | −7.9% |
| pickAll over 40k | 90.81 | 92.02 | +1.3% |
| link 10k tasks to one project, then destroy it | 959.15 | 871.94 | −9.1% |
| 1,000 creates | 142.26 | 131.40 | −7.6% |
| transact: 1,000 creates as one unit | 25.79 | 26.09 | +1.2% |
| 1,000 creates, then 100 watches on a page of the type run again | 160.96 | 134.36 | −16.5% |
| 1,000 page reads | 82.08 | 86.06 | +4.9% |

Every other case is within ±4.4%. None is slower past the 10% tolerance; the faster ones are the machine's load moving
between runs, this change reading no fewer pages for values this short.

## Found along the way

apack's watch scheduler hung `settled()` when a commit found a watch past its gap before its timer fired: the flush ran
the watch, saw the timer and resolved no waiter, and the timer then fired on nothing. Loaded machines fire timers late,
which is how the engine suite hung once at Phase 3 and again here. Fixed in apack `a1b6603e5a` with a fake-clock case
that holds the clock past the due time without firing the timer.
