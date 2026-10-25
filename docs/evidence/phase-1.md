# Phase 1: what the protocol costs

2026-10-25, MacBook Pro (Apple silicon), macOS 25.3, Node 23.11.1, Rust 1.99.1, heed 0.23.0, lmdb-js 3.5.9. The spike
(`spike/`, a throwaway napi crate) was deleted at the end of the phase; this record is what is kept of it.

## Data

The production backup of 2026-10-17 (`agentbuddy-backup-2026-10-17T23-06-18-232Z`, format 1), copied and converted to
format 4 with apack's `scripts/ears-convert.ts` at `fe3785fb0`: 22,816 entities, 178,131 attributes, 45,798
relations, 406,969 entries, 372 MB uncompacted. The backup was only read.

## Stock LMDB can't open lmdb-js's files

heed refused the converted copy: `MDB_INVALID: File is not an LMDB file`. The meta page shows why:

```
$ xxd -l 64 data.mdb
00000010: 0000 0800 0000 0000 dec0 efbe 0200 0000   magic 0xBEEFC0DE, data version 2
```

| LMDB | `MDB_DATA_VERSION` |
|---|---|
| upstream `mdb.master` (0.9, heed's) | 1 |
| lmdb-js 3.5.9's fork | 2 |
| upstream `mdb.master3` (1.0) | 3 |

So the copy went through `tools/lmdb-js-export` (406,969 entries, a 196 MB dump) and was loaded into a stock LMDB
environment entry by entry, in key order (207 MB). apack's engine read the lmdb-js copy and the spike read the stock
one: the same entries, in two containers. The move of apack's data takes the same two steps (`format.md`, D12).

## Readings

Each case ran in one process, after one warm-up, as the median of 5; every elmdb answer was checked equal to apack's
(`isDeepStrictEqual`) before it was timed. `npm run check:idle` read 71% idle when the run began and 51% when it ended;
the warm readings came first and are clean, and the later cold `getRow` rounds may have been disturbed (a `git` fetch
of this session's started during them).

**Warm**, milliseconds:

| Case | apack today | elmdb plan |
|---|---|---|
| every `Message` row, 19,803 (`pickAll`) | 569.2 | 430.2 decoded as apack decodes (maps, then objects); 351.8 decoded straight to objects; 98.6 the plan alone, as bytes |
| `getRow` of 1,892 ids across types | 51.6 | 34.6 in one plan; 37.0 a plan per id |
| `Message` `where('sender', 'user')`, walking (the copy's indexes are unbuilt) | 37.9 | 12.5 |
| `Message` `count()` | 0.017 | 0.0016 |
| `Message` `first()` | 0.019 | 0.0018 |
| `out('contains')` from 343 threads, trashed far ends skipped | 49.1 | 16.7 |

**Cold** (each round on a sparse disk image detached and attached again; every round listed), milliseconds:

| Case | apack today | elmdb plan, decoded |
|---|---|---|
| every `Message` row | 3,236.8, 1,237.6, 1,237.7, 1,126.1, 963.2 | 943.2, 630.7, 674.1, 638.6, 635.8 |
| `getRow` of 1,892 ids | 271.5, 260.3, 449.1, 256.0, 263.0 | 320.0, 226.3, 210.2, 218.6, 224.0 |

The cold comparison also compares containers: the lmdb-js copy holds the old formats' freed pages (372 MB) and the
stock one is dense (207 MB).

## What it decided

- **D4 stands.** A plan and one decode is faster than apack today on every case, warm and cold, so the envelope wasn't
  redesigned and the napi object crossing wasn't needed.
- **Decoding is most of the long read's cost**: 332 of its 430 ms. Two levers follow, for Phase 5: a plan returns only
  the fields it asks for, and a result is decoded straight to objects when no map in it has a `__proto__` key (the
  engine can flag that as it writes the envelope), the exact decode kept for the rest.

## The values apack stores

Every value under format 4's `['v', id, field]`, decoded with rmpv (178,131 values): 536,140 strings, 220,933 maps,
72,312 float64s, 46,135 booleans, 25,055 non-negative integers, 18,607 arrays, 13,705 nils. No extension type, no
binary, no float32, no negative integer, no map key that isn't a string, no trailing bytes. The value spec in
`format.md` follows from it.

## ITS#10642

Upstream's fix (19d444f, 2026-10-19) changes `mdb_subdb_adjust`, which heed 0.23's LMDB doesn't contain (0 matches in
its `mdb.c`): the page-count code it fixes arrived upstream after heed's base. Its call sites are DUPSORT
sub-databases (`F_SUBDATA`), which elmdb doesn't use either.

## LMDB's assertions in a release build

`lmdb-master-sys`'s build defines `NDEBUG` outside debug builds, which compiles LMDB's `mdb_assert0` to nothing: a
release build never aborts on an LMDB assertion. heed also keeps the raw `MDB_env` pointer crate-private, so
`mdb_env_set_assert` can't be called. The crash record `engine.md` designed has no subject: D22.
