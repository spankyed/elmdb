# Freezing format 1

Status: designed 2026-11-16, not built. This is change 8 of the road to 1.0 (`../roadmap.md`): databases and dumps of
every tag open and pass `check()` in CI, and the move to LMDB 1.0 is decided first. It decides that move, says what
the freeze locks, and orders the work.

## Where it starts

- `m/format` holds 1, and an open refuses any other (`WRONG_FORMAT`; `check_stamp`, `store/mod.rs`). Format 1 is edited
  in place until a release has users. From that release's tag it is locked, and every change to the stored layout
  bumps it and ships an upgrade (`../architecture/format.md`, Versions).
- A dump holds the schema and the primary data as loads, and names no key or stored value (dump version 1, `dump.rs`).
  Its footer holds the dumped database's last commit sequence, and a load's last commit is numbered past it, so a
  `changedSince` position read at or before the dump is told to start over (`../architecture/format.md`, Dumps). It
  doesn't hold the other meta keys: the replacement count (`m/epoch`), the tombstones (`l/d`) or the checkpoints
  (`l/k`).
- On 2026-11-16 the repository has no tag, and its packages are at 0.0.0.
- elmdb is on heed 0.23, whose LMDB is upstream `mdb.master` (0.9) with ITS#10395. A data file records LMDB's data
  version: 1 for 0.9, 3 for 1.0 (`../evidence/phase-1.md`), and each LMDB refuses the other's file
  (`MDB_VERSION_MISMATCH`, `mdb.c`).

## LMDB: format 1 is on 0.9

Read on 2026-11-16:

- LMDB 1.0 is released: 1.0.1 on 2026-08-06, and 1.0.3 tagged on 2026-10-21. Both write data version 3 (`mdb.c` and
  `lmdb.h` at the tags `LMDB_1.0.1` and `LMDB_1.0.3`).
- heed3 0.23.1 is heed on `lmdb-master3-sys` 0.2.6, published 2026-04-07 (crates.io). That crate holds an `mdb.master3` from
  before the release, its `lmdb.h` still 0.9.90. It writes data version 3 too, but it is no release.
- hannoy 0.3.0, which holds a vector index's graph, depends on heed and has no feature for heed3. elmdb can't take
  both: they are two LMDBs whose C functions have the same names, and principle 5 holds one per process
  (`cargo xtask check-one-lmdb`).
- What 1.0 adds is in no gate of the road. Among it, `lmdb.h` at `LMDB_1.0.3` declares encryption
  (`mdb_env_set_encrypt`) and incremental backup (`mdb_env_incr_dump`). The road has it under Later.

The move now would mean forking hannoy onto heed3 and running an LMDB that isn't a release, for what no gate asks.
Format 1 is on LMDB 0.9.

**If the move comes later.** Before the first release with users, it is one more in-place edit of format 1, crossed by
a dump and a load as every edit is now. Only after that release would `format.md`'s rule make it a new format. Either
way, no build can hold both LMDBs, so it takes two builds: the last on 0.9 dumps, and the first on 1.0 loads. A dump holds no
key, no value's encoding and no LMDB page, so it crosses the change as it crosses every in-place edit today. The
`synced` record beside the data file holds an LMDB 0.9 meta page, so it changes with the format.

The costs:

- The last 0.9 build's `elmdb dump` stays installable for as long as a format 1 database is to be moved.
- A load builds every derived key again, as a `rebuild()` does.
- A reader that follows the database by sequence starts over: a load's last commit is numbered past the dumped
  database's, and marked as one that replaced every type whole (`changed::replaced`, as a reset
  does). A reader that keeps its place in a checkpoint loses it, since a dump holds none. So before the lock, a dump
  carries the checkpoints too. The dump stays version 1, edited in place as the format is while no release has
  users.
- The move is of a database no one writes from the dump on. A position taken from the old database after the dump,
  were it written to, could be past the load's last sequence and answered complete from the new numbering. Nothing
  in a position names its database.

**When to look again:** once hannoy runs on heed3, and heed3's LMDB is a 1.0 release.

## What the freeze locks

From the first release with users, format 1 locks:

- the key encoding (`format/key.rs`), and the keys and values of every primary and meta prefix: `p/v`'s msgpack as
  given, and `0xc1` and a zstd frame for a compressed one; `p/i`, `p/r`, `p/s`, `p/q`, `m/` and `l/`;
- the derived keys that belong to no index: `d/e`, `d/t`, `d/c`, `d/h`, `d/rc`, `d/f`, `d/o`, `d/n` and `d/b`. Plans
  read them for every type and relation kind, and nothing answers in their place while they are built. A change to
  them bumps the format, and its upgrade is a `rebuild()` on a copy;
- each index's marker, its key and its encoding (below), and with it the place a build of the index has reached
  (`d/xbr`), which every writer of the marker deletes. An elmdb from before that key (#189, 2026-11-18) deletes
  none, so one that rebuilt or removed an index would leave a place for a later elmdb's build to go on from, past
  rows it never filed. No such elmdb is released, and the lock is what keeps one from being: from the first tag on,
  an elmdb that says whether an index is built takes its place away;
- the files beside the data file: the `synced` record and the locks (`format.md`, Beside the data file);
- the dump file, version 1;
- LMDB's data version, 1.

Index-derived data stays out of the lock, as principle 2 already allows: anything derived can be thrown away. Each
index records the layout it was built in, and one built in another layout is thrown away and built again.

A design that moves a locked key is a format of its own, or lands before the lock. The text index's Order 4 does
this, since it moves a type's entities in creation order into a database of their own (`text-index-at-scale.md`).

## Each index's layout

What an index holds depends on elmdb's code, on the crates whose output it stores, and on the Unicode tables the
toolchain brings:

| Index | Its keys | What else shapes them |
|---|---|---|
| value | `d/x`, `d/xp`, `d/xw`, `d/xwp`, `d/xu`, `d/xup`, `d/xl`, `d/xlp` | nothing |
| an order of strings | `d/xo`, under its marker `d/xob` | icu4x's collator, already in the marker's stamp |
| text | `d/ft`, `d/fb`, `d/fd`, `d/fl`, `d/fs`, `d/fw` | charabia and rust-stemmers; what charabia normalizes, detects a language and segments by (unicode-normalization, whatlang, finl_unicode); and its matching crates, aho-corasick and slice-group-by |
| trigram | `d/g` | the toolchain's Unicode version, through `char::to_lowercase` (`engine/trigram.rs`) |
| vector | `d/qc`, `d/qp`, `d/qi`, `d/qm`, `d/qj`, `d/qn`, and the graph databases | hannoy |
| rank | `d/ra`, `d/rk`, `d/rq`, `d/rs` | nothing |

Today `d/xb` holds `true`: an index is built or it isn't. The crates above, but for charabia's matching crates, are
required at exact versions (`INDEXED`, `cargo xtask check-exact`), so no `cargo update` takes a new release of one
unasked. Taking one on purpose
changes what an index holds all the same:

- **A tokenizer release that splits or stems a word differently:** a query no longer finds the terms the index holds
  for it, and `check()` reports a sound database as differing.
- **A hannoy release that lays out its graph differently:** reads misread a graph the release before built.

The design takes from `d/xob`, the marker of an order of strings (`../archive/roadmap/string-order.md`). A write
consults that marker before it files, and one of another stamp is deleted and nothing is filed (`order::filing`). Its
keys sit under a build, so no write mixes two layouts. A marker of `true` gives none of that: a write keeps an index's
keys whether or not it is built, and reads what is stored to do it (`text::file` decodes `d/fd`), and a build files from
the first place without clearing first (`build_index`, `engine/builder.rs`). So:

1. **Each index has a marker of its layout:** `[number, hash, state]`.
   - `number` is elmdb's layout number for the index's kind.
   - `hash` covers the exact versions of what shapes it, and the output for a few probe strings, as `order::stamp_of`
     has them for a collator. The versions are those `INDEXED` holds exactly (`cargo xtask check-exact`), as the
     order's `CRATES` lists icu4x's. The probes catch what no version records, such as charabia's matching crates,
     which it requires as ranges.
   - `state` is clearing, building or built.
   - Value, text, trigram and vector indexes keep it in `d/xb`, written through a new helper in place of the seven
     places that write `true` today.
   - A rank index keeps it in a key of its own, under a prefix declared in `format/mod.rs`'s table and here, since
     `Totals::decode` refuses a `d/rs` of any other length, which would make an older build answer `CORRUPTED`. Reads
     gate on that marker. `Totals.stage` goes on saying how far the build in the marker's layout has come.
   - A marker that doesn't decode reads as another layout, as `order::marker` reads one.
2. **Every write that files into an index reads its marker first**, as a write into an order does (`order::filing`).
   That covers:
   - a field's refiling;
   - an edge's change, which files into rank indexes (`edge_changed`, `relations.rs`);
   - an entity's liveness in a rank index (`ranks::liveness`);
   - an embedding written or restored (`vector::embed`, `vector::restore`) and the graph's changes at commit
     (`vector::flush`);
   - the builder's rank drain.

   A unit reads each index's marker once and keeps it, so this costs about what `order::filing` does.
3. **A marker of another layout, with a number no greater than this build's, starts the index over.**
   - The write or open that finds it puts this build's marker in its place, set to clearing, and files nothing into
     the index, as `order::filing` deletes a marker of another stamp and files nothing.
   - The builder then clears what the index holds, in chunks as it builds: every derived prefix its kind has had,
     the retired ones among them (`Prefix::RETIRED`), and its graph. It keeps the embeddings (`p/q`), which are
     primary.
   - This is a clearing of its own. The path an index dropped from the schema takes (`drop_index_data`,
     `engine/mod.rs`) deletes `p/q` too, and collects every key in one transaction, which at a million notes would
     put a text index of 131 MB into one write (`text-index-at-scale.md`, Order, read on 2026-11-10).
   - Once cleared, the marker is set to building, and the builder builds the index as it builds a new one.
   - Every unit of a clearing or a build holds the marker to the number, hash and state it began under, and stops
     when it is another. An order's build checks the same, and goes back to its beginning instead
     (`order::build_unit`). Without that, a build of one
     layout would go on filing after another build had started the index over, and its last unit would mark the
     index built.
   - An open queues a marker of its own layout that is clearing or building, as `unbuilt_indexes` queues a missing
     one today: a backup taken during a clearing, or a crash during one, leaves such a marker. A read-only handle
     writes no marker, and answers as in 5.
4. **A marker with a greater number is refused, and nothing is cleared.**
   - An open, a write that files into the index and a read of it are refused with a new code,
     `INDEX_LAYOUT_NEWER`, naming the index and saying to update elmdb.
   - `WRONG_FORMAT` means the database is in a format this build doesn't read, and a code never changes meaning.
   - An older build can't clear what it has never heard of, such as a later layout's journal or databases.
   - With two builds open on one database, the newer one starts the older one's index over, and the older one then
     refuses rather than file into it.
   - Every other writer of a marker refuses one with a greater number the same way: a `rebuild()` of everything or
     of one index, which clears markers and writes them built, and the marking of an empty type's indexes built
     (`settle_indexes_in`).
   - Two builds with the same number and different hashes, such as one app's builds before and after a tokenizer
     update, both open at once, take turns starting each other's index over. Each turn is a clearing and a build by
     the builder, never inside a caller's write. Nothing refuses that: refusing a different hash under an equal
     number would leave a database that a dependency update makes unwritable.
5. **An index whose marker has a number no greater than this build's, and isn't this build's layout in the built
   state, answers as an index being built does**
   (`../architecture/engine.md`, Reads):
   - a plan over a value or trigram index walks the rows in its place;
   - a search over a text index is refused (`INDEX_BUILDING`);
   - a vector index's reads are refused the same way: `vector::nearest` reads the graph whatever the marker says today,
     and this adds the check;
   - a rank index computes its scores, or is refused under `whileBuilding: "refuse"`.

   `check()` compares an index only once it is built in the running layout, and adds a note naming each index it
   didn't compare, as it names an order today. Today `check_items` and `ranks::violations` read every vector and
   rank index, so this adds the check there too.

After the lock, a change to an index's keys then changes its kind's layout number, and needs no new format. The text
index's journal and vectors on it (`text-index-at-scale.md`, Order 2 and 3) are such changes, so they can land after
the freeze. Change 6 stays in milestone 0.5 instead of holding up 0.3.

The cost: after an update that changes a layout, its indexes answer as in 5 until the builder has built them. A
`rebuild()` of every derived key at a million notes took 36.4 s to 59.0 s in the current layout
(`../evidence/million-notes.md`, 2026-11-10, Apple M1 Pro, under the machine loads that file names), and one index is
a share of that.

## The compatibility test

- **The fixture.** `cargo xtask format-fixture` writes a database and its dump from a fixed workload, together with the
  answers its reads give.
  - The workload writes under every primary and meta prefix in `format/mod.rs`'s table, and builds an index of each
    kind. A test fails when the table holds a primary or meta prefix the workload writes nothing under.
  - It writes each encoding the lock holds:
    - numbers negative, fractional, zero and −0;
    - strings with zero bytes in them, and long ones;
    - byte parts in keys;
    - compressed values;
    - weighted relations, and trashed and destroyed entities;
    - a checkpoint.
  - It holds hundreds of entities, so a fixture stays small.
- **The fixtures.** One pair for the commit that locks format 1, and one for each tag after it, committed under
  `crates/elmdb/tests/format/`. Each pair is a fixture written on Linux x64, whose pages are 4 KB, and one written
  on Apple silicon, whose pages are 16 KB. LMDB takes an existing file's page size from its meta page (`mdb.c`), and
  the test shows each opens where the other was written.
- **What CI does with them.** On every platform CI runs:
  - a copy of each fixture's database opens and waits for its index builds, since a layout changed since the tag
    leaves its indexes to be built again;
  - it passes `check()`, and answers the workload's reads as committed;
  - its dump loads into a database that does the same.

Milestone 0.3's gate asks the same of the protocol and the error codes. Those are change 7's, and not designed here.

## Order, each with its gate

1. **This decision**, with `format.md`'s LMDB section and `design.md` naming it.
2. **An index's layout in its marker.**
   - Under a forged layout, as `order.rs` forges a stamp, a lesser number's index:
     - files nothing from any write path in 2 until cleared;
     - keeps its embeddings;
     - is built again by the builder;
     - and `check()` is clean after.
   - A greater number is refused at an open, a write, a read, a `rebuild()` and the marking of an empty type's
     indexes with `INDEX_LAYOUT_NEWER`, and its keys are left as they were.
   - A build's unit stops when another build has started the index over between its units, and its index isn't
     marked built.
   - An open queues a marker of its own layout left clearing or building, and the builder finishes it.
   - A crate in `INDEXED` at another version gives another hash, and its indexes are started over.
3. **What a dump carries.** A `changedSince` position read at or before the dump starts over (built,
   `../architecture/format.md`, Dumps). Left: a load keeps the checkpoints.
4. **The fixture and its test**, shown failing once on a mutated copy that changes a primary key's encoding.
5. **The lock:** `format.md`'s Versions names the tag of the first release with users. The tag is the owner's.

## What was weighed

- **Moving to LMDB 1.0 first.** It forks hannoy and runs an LMDB that isn't a release. It is weighed again when the
  trigger above holds.
- **Locking index-derived data with the rest.** The journal, in milestone 0.5 with change 6, would have to land
  before milestone 0.3, and every tokenizer update would be a new format.
- **A marker that only makes an index unbuilt, leaving its keys.** A write reads stored keys to file new ones, and a
  build doesn't clear first, so an index would mix two layouts.
- **Clearing a stale index in the write that finds it.** One caller's write of one entity would delete a whole index
  in its own transaction.
- **One build that reads both LMDBs and upgrades in place.** The two LMDBs' C functions have the same names, and
  principle 5 forbids two LMDBs in one process.
- **Rebuilding the derived keys of no index on open after a change.** An open would hold its caller for a `rebuild()`
  of everything, 36 s and more at a million notes. An upgrade on a copy keeps that out of an open, as `format.md`
  has it.
