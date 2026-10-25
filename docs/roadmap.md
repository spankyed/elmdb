# Roadmap

The order of work. Each step ships something apack uses. Status: not started.

## 1. Spike: what the protocol costs

Whether to build is decided: apack is moving off lmdb-js, and search needs indexes that commit with the data. The spike
measures the one cost the design adds, encoding a result and decoding it in JavaScript, against apack today on a copy
of its converted production database (format 4, 406,969 entries, 22,816 entities):

- every `Message` row (19,803 entities), about 540 ms in apack today;
- `getRow` of 2,000 entities across types;
- an indexed `where`, a type scan with `count` and `first`, and a relation walk.

Each warm and cold (the copy on a disk image detached and attached again), medians of five, on a machine checked quiet
first. If the plan with decoding isn't faster than today on the long read, the result envelope is redesigned before
step 2.

## 2. Parity, and apack off lmdb-js

The graph, value indexes, units, savepoints, numbering, refusals and durability, behind apack's unchanged `Ears` API.
apack's model harness drives its JavaScript engine and elmdb with the same workloads and compares both with the model
and with each other, and the crash harness runs against elmdb. apack switches when they agree, and drops lmdb-js.
apack's side is `docs/plans/ears-on-elmdb.md` in apack.

## 3. The `runs` database

apack's run history moves from its record store into a second database. `RecordStore` goes away.

## 4. Text search

Ranked full text with filters and highlights, then fuzzy labels for a quick-open (`search.md`).

## 5. Change sets and watch

Live queries in apack's views.

## 6. Vector and hybrid search

Replacing apack's dormant library index.

## Later, each its own decision

Directions past these, each a design to argue for before it is built, are in `roadmap/` (`roadmap/README.md`).


- Value compression with per-type dictionaries.
- `elmdb-cli`.
- LMDB 1.0 through `heed3`, once released and supported by hannoy: page checksums and encryption.
- Substring and regex search.
- `orderBy` with `limit` as a top-k heap, or an ordered index scan, rather than a sort of the whole set.
- Fewer reads per field write, and no global sequence key rewritten by every insert.
