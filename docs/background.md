# Background: why elmdb exists

What working with lmdb-js in apack taught, as of 2026-10-24.

## apack's storage

apack's EARS engine (`@apackdev/ears`) stores everything in LMDB through lmdb-js, with no copy of the data in memory:
the engine reads and writes the database directly, behind an ordered key-value seam (`OrderedKV`). On apack's
production data that's 22,816 entities, 178,131 attributes and 45,798 relations.

## A crash in lmdb-js's fork of LMDB

A compacted copy of that database (`backup(dir, true)`) aborted the process the first time it was emptied:

```
mdb.c:2953: Assertion 'key.mv_size > 0' failed in mdb_page_alloc()
```

- **Cause:** a compacting copy writes an empty free-page database but keeps the source's transaction id. The first
  transaction that frees enough pages to need overflow pages for its free list (clearing a named database of
  multi-page values does) reaches `mdb_page_alloc` while `mdb_freelist_save` has made the free-page database's root an
  empty leaf. lmdb-js's allocator then scans that database backwards, lands on the empty leaf, reads no key, and
  asserts.
- **Fix:** kriszyp/lmdb-js#381, which stops the scan when the free-page database has no entries.
- **Where the bug lives:** in lmdb-js's own allocator. Upstream LMDB (`mdb.master`) has no reverse scan, no
  `me_freelist_*` bookkeeping and no such assertion; it reads the free-page database forward only.

apack turned compaction off behind a flag (`COMPACT_COPIES`) until a release ships the fix.

## What that showed about lmdb-js

- **Actively maintained, by essentially one person:** 58 commits and 11 releases in the twelve months to October 2026,
  52 of those commits by its author. It's the storage engine under Harper.
- **A fork of LMDB, not a binding to it:** its vendored `mdb.c` carries about 3,000 lines that stock LMDB doesn't,
  including its own free space allocator. The crash was in that code, which only lmdb-js's test suite checks.
- **Aborts take the process down:** a failed assertion calls `abort()`, which nothing in JavaScript can catch.
- **The fork keeps moving:** its vendored `mdb.c` changed 55 times in 2022, 5 in 2023, 65 in 2024,
  9 in 2025 and 6 so far in 2026, in bursts.

## What apack does in the meantime

- Pins lmdb-js, and moves the pin only for a fix it needs or a Node/Electron ABI change.
- Uses only a small subset: synchronous transactions, binary keys and values, no caching, overlapping sync off.
- Gates each upgrade on its model and crash harness.
- Keeps the backend behind `OrderedKV`, so moving to another binding is one backend file.

## Why a new engine rather than a patch

Patching lmdb-js fixes one bug in a fork that will keep diverging from LMDB. And apack needs more than a binding:
search whose indexes commit with the data, live queries, and its hot paths out of JavaScript. Once the engine itself
runs in Rust, JavaScript never needs LMDB at all, so elmdb is an engine on heed's LMDB (`design.md`) rather than a
binding for JavaScript to build one on.
