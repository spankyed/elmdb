# A million notes: readings

The evidence for the road to 1.0's benchmark (`../roadmap.md`, change 10) and for the text index's design at scale
(`../roadmap/text-index-at-scale.md`). 2026-11-10. Machine: Apple M1 Pro, 10 cores, 32 GiB, 16 KB pages, Node 24.16.0,
the binding built in release, no engine syncing. The times are readings under the load named with each; the counts and
the bytes are the same on any run.

## Against SQLite

`node crates/elmdb-node/bench/against-sqlite.mjs --json` (`../architecture/testing.md`, Against SQLite) on
`session/million-notes-benchmark`, whose engine is `79f646f`'s: a million notes of six terms each, a number and a
status, in units of 5,000; SQLite 3.53.0 with FTS5 and WAL. Load 6.4 as it began, 4.9 as it ended. The two answered
every read alike.

| | elmdb | SQLite |
|---|---:|---:|
| Load | 132.5 s | 33.7 s |
| Bytes a note, as loaded | 1,812 | 151 |
| Bytes a note, compacted | 1,738 | 145 |
| A create of three terms every note holds, at 50,000 notes | 0.116 ms, 133 µs of CPU | 0.037 ms, 80 µs |
| The same at a million | 0.374 ms, 404 µs | 0.054 ms, 59 µs |
| A create of thirty terms, fifteen widely held, at 50,000 notes | 0.332 ms, 372 µs | 0.039 ms, 50 µs |
| The same at a million | 1.544 ms, 1,601 µs | 0.059 ms, 71 µs |
| The best ten for a word every note holds | 3,630 ms | 550 ms |
| Its count | 3,642 ms | 26.9 ms |
| The best ten for a word a twelfth hold | 311 ms | 51.0 ms |
| Its count | 311 ms | 2.2 ms |
| A misspelled word | ten found, 630 ms | none found, 0.017 ms |
| The ten lowest of the indexed number | 769 ms | 0.006 ms |
| The last 500 of it | 1,017 ms, filtered by the caller | 0.005 ms |
| A page of 50 near the end of creation order | 191 ms | 0.017 ms |
| A check | 41.5 s | 2.3 s |
| A rebuild | 85.3 s | 2.1 s |

Both of elmdb's limited reads visited every note: 1,000,082 entities for the best ten of the word every note holds,
and as many for the ten lowest of the number. SQLite's check is `PRAGMA integrity_check`, and its rebuild is its text
index's `rebuild` and `REINDEX`. A commit's times are medians of 41, and its CPU is the process's over the 41.

## The engine, by what a create holds

A disposable script through the binding at `0f35f0c`, whose tree `79f646f` merged: a type whose title is in a text
index and nothing else, a million notes of the titles above, then creates of one note each, medians of 41 taken in
turn so a drift falls on every shape alike. Load about 4.

| The create holds | Its commit |
|---|---:|
| a title, in a type with no index | 0.060 ms |
| 1 term no note holds | 0.080 ms |
| 1 term every note holds | 0.137 ms |
| 30 terms no note holds | 0.190 ms |
| 30 terms that exist, rare and scattered | 0.417 ms |
| 30 terms: 15 no note holds, 12 a twelfth hold, 3 every note holds | 1.057 ms |
| 30 terms that exist: 15 widely held, 15 rare and scattered | 1.309 ms |

A widely held term cost 8.5 µs of a commit at 50,000 notes, 22 µs at 250,000 and 75 µs at a million.

**Where a ranked search's time goes**, read on the same database from LMDB directly, for the word every note holds
(a million matches): walking the term's bitmap, 4 ms; reading and decoding each match's document record, 1.5 s; the
term's positions in it, 0.9 s; its entity's record, as the plan reads it to admit the hit, 0.7 s. Scoring every match
from two arrays held in memory, a count and a length, and keeping the best ten: 4.5 ms.

**Where the bytes go.** The same database, each key family's entries summed from LMDB's own pages: 1,330 MB of file for
526 MB of keys and values, its leaves half full. Every entry appended in key order to a new database: 656 MB.

| Keys | Entries | Keys | Values |
|---|---:|---:|---:|
| `d/fp`, a term's positions in an entity | 5,508,118 | 177.8 MB | 26.7 MB |
| `p/v`, a field's value | 3,002,583 | 82.1 MB | 51.6 MB |
| `d/fd`, an entity's document | 1,000,738 | 25.8 MB | 47.9 MB |
| `d/ft`, a term's places | 1,006,042 | 24.8 MB | 32.5 MB |
| `d/e`, a type's entities in creation order | 1,000,861 | 19.1 MB | 12.4 MB |
| `p/i`, that an entity exists | 1,000,861 | 18.1 MB | 7.5 MB |

A vector index of 20,000 embeddings of 384 values, 29.3 MB as numbers, is a file of 124.6 MB: the embeddings (`p/q`)
31.1 MB, the graph's copy of each 29.4 MB, the chunks' hashes and text (`d/qc`) 8.9 MB, and 85.8 MB in all were every
page full.

## The layouts

A disposable program on LMDB through `heed`, with no engine: a million documents of one text field, each of the
layouts written as `../roadmap/text-index-at-scale.md` describes it, the terms synthetic. A commit is of one message
of thirty terms that exist, fifteen widely held and fifteen rare and scattered, a median of 41. The pages a commit
writes are how far the file grew with a reader holding the pages before it. Load 3 to 7.

| Layout | Built in | The index | A commit | Its pages |
|---|---:|---:|---:|---:|
| A term's places whole, positions a key per term and entity, as today | 5.8 s | 372 MB | 0.83 ms | 211 |
| A term's places in blocks of 2 KB with a count each, written by the commit | 1.8 s | 157 MB | 0.15 ms | 45 |
| The same blocks, the commit writing the document's terms and a later merge the blocks | 1.8 s | 157 MB | 0.014 to 0.025 ms | 7 to 9 |
| The same blocks, the commit writing the place into a journal | | | 0.011 ms | 5 |

- **The later merge**, a message: 0.102 ms merging 100, 0.043 ms merging 1,000, 0.029 ms merging 10,000, which held the
  writer for 292 ms.
- **A unit of 1,000 messages**, a message: 0.44 ms with the places whole; 0.054 to 0.056 ms into blocks in the order
  written; 0.046 to 0.053 ms into blocks in term order, which wrote 5.1 to 6.2 pages a message where the order written
  wrote 3.4 to 3.5.
- **Ranking from the blocks and the lengths as stored**, the best ten kept: a term every document holds, a million
  matches, 2.83 ms; a term a twelfth hold, 83,334 matches, 0.55 ms.
- **Tokenizing**, with the engine's tokenizer: a message of 30 words, 188 bytes, 10.5 µs; a reply of 2,645 bytes,
  137.6 µs.
- **Pages.** Two families of 400,000 keys of 9 bytes with values of 60, written in turn, each key past its family's
  last: 65% of the leaves' bytes used in one database, 98% with a database each.
- **A scan of one bit a value**, keeping the nearest 200, no recall read: 384 values, 0.39 ms over 100,000 vectors and
  3.6 ms over a million; 1,536 values, 0.8 ms and 7.5 ms.

What these don't show: the engine, which the next section reads with what of this is built; pages of 4 KB; text with
a natural spread of terms; or more than one text field.

## With a term's places in blocks

The same readings once a term's places are in blocks with their counts, lengths are kept by place and a search is
ranked from the two (`../architecture/search.md`, Text search as built), on `session/text-blocks-and-ranking` at
`82bf5f8`, the same machine and day.

**Against SQLite**, the benchmark as above. Load 11.6 as it began, minutes after the machine started, and 3.2 as it ended. SQLite's readings are
its own of this run.

| | Before | Now | SQLite |
|---|---:|---:|---:|
| Load | 132.5 s | 56.6 s | 32.9 s |
| Bytes a note, as loaded | 1,812 | 1,174 | 151 |
| Bytes a note, compacted | 1,738 | 1,186 | 145 |
| A create of three terms every note holds, at 50,000 notes | 0.116 ms, 133 µs of CPU | 0.116 ms, 131 µs | 0.036 ms, 75 µs |
| The same at a million | 0.374 ms, 404 µs | 0.172 ms, 186 µs | 0.057 ms, 61 µs |
| A create of thirty terms, fifteen widely held, at 50,000 notes | 0.332 ms, 372 µs | 0.228 ms, 266 µs | 0.038 ms, 49 µs |
| The same at a million | 1.544 ms, 1,601 µs | 0.425 ms, 464 µs | 0.061 ms, 74 µs |
| The best ten for a word every note holds | 3,630 ms | 20.6 ms | 556 ms |
| Its count | 3,642 ms | 0.35 ms | 25.2 ms |
| The best ten for a word a twelfth hold | 311 ms | 4.5 ms | 50.5 ms |
| Its count | 311 ms | 1.9 ms | 2.1 ms |
| A misspelled word | ten found, 630 ms | ten found, 18.1 ms | none found, 0.018 ms |
| The ten lowest of the indexed number | 769 ms | 750 ms | 0.006 ms |
| The last 500 of it | 1,017 ms | 983 ms | 0.005 ms |
| A page of 50 near the end of creation order | 191 ms | 191 ms | 0.017 ms |
| A check | 41.5 s | 34.3 s | 2.0 s |
| A rebuild | 85.3 s | 36.4 s | 2.0 s |

The best ten of the word every note holds visited ten entities, where they visited 1,000,082. The ten lowest of the
number, which this changed nothing of, still visit every note.

**By what a create holds**, the script as above, in a database of the text index alone. Load 2.4.

| The create holds | Before | Now |
|---|---:|---:|
| a title, in a type with no index | 0.060 ms | 0.057 ms |
| 1 term no note holds | 0.080 ms | 0.073 ms |
| 1 term every note holds | 0.137 ms | 0.067 ms |
| 30 terms no note holds | 0.190 ms | 0.119 ms |
| 30 terms that exist, rare and scattered | 0.417 ms | 0.195 ms |
| 30 terms: 15 no note holds, 12 a twelfth hold, 3 every note holds | 1.057 ms | 0.198 ms |
| 30 terms that exist: 15 widely held, 15 rare and scattered | 1.309 ms | 0.272 ms |

**Where the bytes are**, that database's keys and values summed as above, read before the branch's last changes, none of
which is to what is stored: 644 MB of file where there were 1,330, for 322 MB of keys and values where there were 526.
Its leaves are still half full.

| Keys | Entries | Keys | Values |
|---|---:|---:|---:|
| `d/fd`, an entity's document, its positions in it | 1,000,328 | 25.8 MB | 65.5 MB |
| `p/v`, a field's value | 3,001,107 | 82.1 MB | 51.6 MB |
| `d/ft`, a term, and its places while 256 or fewer hold it | 1,002,024 | 24.7 MB | 4.8 MB |
| `d/e`, a type's entities in creation order | 1,000,369 | 19.1 MB | 12.4 MB |
| `p/i`, that an entity exists | 1,000,369 | 18.1 MB | 7.5 MB |
| `d/fb`, a block of a term's places | 20,517 | 0.7 MB | 8.6 MB |
| `d/fl`, the lengths in a block of places | 1,954 | 0.1 MB | 1.0 MB |

The text index's own keys and values are 131 MB where they were 336 MB: the documents grew by the positions they now
hold, 18 MB, and the 204 MB of a key per term and entity are gone.

**Where a ranked search's time goes now**, sampled over the best ten of the word every note holds, 200,000 notes
(`cargo run --profile profiling -p elmdb --example search-cost -- 200000 3000`, macOS `sample`): about two fifths in
the loop that joins a query's clauses around each match, a sixth reading the term's entries in place order, a sixth
walking the set of matches, a seventh keeping the best ten, and a twentieth weighing counts. It comes to about 20 ns
a match, 3.9 ms for the 200,000, where the layouts' reading above scored one term's entries in a loop of its own at 3.

## With orders, ranges and pages read from a value index

The same benchmark once an `orderBy`, a `range` and an `after` read a value index (`../architecture/engine.md`, Reads:
plans), on `session/ordered-reads` at `e781d4d`, the same machine and day. It reads two things it didn't: the ten highest of the
number, and a page of fifty near the end of the number's order. The machine was busy with other work: load 25.7 as it
began and 25.5 as it ended, which the seconds below carry. SQLite's readings are its own of this run.

| | Before | Now | SQLite |
|---|---:|---:|---:|
| The ten lowest of the indexed number | 750 ms | 0.032 ms | 0.006 ms |
| The ten highest of it | | 0.038 ms | 0.005 ms |
| The count of the last 500 of it | 983 ms, filtered by the caller | 0.020 ms | 0.005 ms |
| A page of 50 near the end of creation order | 191 ms | 0.043 ms | 0.018 ms |
| A page of 50 near the end of the number's order | | 0.063 ms | 0.032 ms |
| Load | 56.6 s | 69.0 s | 52.5 s |
| Bytes a note, as loaded | 1,174 | 1,174 | 151 |
| Bytes a note, compacted | 1,186 | 1,186 | 145 |
| A create of three terms every note holds, at a million | 0.172 ms | 0.149 ms | 0.068 ms |
| A create of thirty terms, fifteen widely held, at a million | 0.425 ms | 0.324 ms | 0.085 ms |
| A check | 34.3 s | 51.7 s | 2.3 s |
| A rebuild | 36.4 s | 59.0 s | 2.1 s |

What each read visited, which is the same on any run: ten entities for the ten lowest and for the ten highest, where
the ten lowest visited 1,000,082; 500 index entries for the count, and no entity; fifty for each page, where the page
in creation order walked to its cursor through 999,800.

The seconds are the load's more than the change's. A run an hour before, at load 11.5 falling to 7.2 and on the same
tree but for the count, which then handed on its 500 entities and took 0.437 ms, read a load of 60.2 s, a check of
39.2 s and a rebuild of 43.8 s, with SQLite's own load at 44.1 s where it had been 32.9. A check and a rebuild read
each entity's value once more for each index that accounts for it, which these loads are too uneven to show.

The number and the status are fields every note holds, so an index's accounting for every entity of its type
(`../architecture/schema.md`, Indexes) added no key, and the bytes are what they were.

## A filter before an order

What `where`, then `orderBy`, then `limit 10` costs, by what the filter keeps: a disposable script through the
binding, a million notes of a number that is a permutation of their places, a status half of them hold, an owner a
thousandth hold, and a tier the top hundredth by the number hold, each in an index of its own; the first ten by the
number, ascending, medians of three and then of five. The same machine and day, load 3 to 5.

On `session/ordered-reads` at `e781d4d`, the two ways a plan had to it, which answer the same. The planner read the
filter first; the order first was a caller's to write.

| The filter keeps | The filter read, its matches sorted | The order walked, each entity tested |
|---|---:|---:|
| Half the notes | 676 ms, 500,000 visited | 0.070 ms, 20 visited |
| A thousandth | 2.3 ms, 1,000 visited | 30.1 ms, 9,434 visited |
| The top hundredth by the number itself | 24.7 ms, 10,000 visited | 3,239 ms, 990,010 visited |

Neither is the cheaper for every filter, and which is depends on the notes, not on the plan. An entity the walk
tests costs about 3.3 µs, its record and the filter's field (30.1 ms for 9,434; 3,239 ms for 990,010), and a match
the sort reads 1.4 to 2.5 µs, its own field (676 ms for 500,000; 24.7 ms for 10,000): the walk's entity costs about
twice the sort's, which is what `WALK_FOR_EACH_READ` in `crates/elmdb/src/engine/reads.rs` says.

On `session/compound-reads` at `bfadcd0`, the plan as written, filter first: with no index on the filter's field and the number
together the planner walks the order while that is the cheaper way (`../architecture/engine.md`, Reads: plans), and
with one it reads it.

| The filter keeps | No index on both | An index on both |
|---|---:|---:|
| Half the notes | 0.081 ms, 20 visited | 0.053 ms, 10 visited |
| A thousandth | 3.4 ms, 1,512 visited | 0.032 ms, 10 visited |
| The top hundredth by the number itself | 51.0 ms, 18,192 visited | 0.024 ms, 10 visited |

With no index on both it is within about twice the cheaper of the two ways in each row, where the dearer was up to
ten thousand times it: the thousandth's 1,512 are the 512 the walk read before it was given up and the filter's
1,000, and the hundredth's 18,192 are 8,192 and 10,000. An index on both reads ten entries whatever the filter keeps.

**Against SQLite**, the benchmark as above with the ten lowest of the open half added, on `session/compound-reads`
at `bfadcd0`, the machine quiet: load 3.6 as it began and 2.5 as it ended. Each engine has an index on the status and
one on the number, and none on both; the benchmark runs no `ANALYZE`, so SQLite's planner chooses without statistics.

| | With a term's places in blocks | Now | SQLite |
|---|---:|---:|---:|
| Load | 56.6 s | 57.7 s | 33.1 s |
| Bytes a note, compacted | 1,186 | 1,186 | 145 |
| A create of three terms every note holds, at a million | 0.172 ms | 0.167 ms | 0.056 ms |
| A create of thirty terms, fifteen widely held, at a million | 0.425 ms | 0.415 ms | 0.060 ms |
| The ten lowest of the indexed number | 750 ms | 0.030 ms | 0.006 ms |
| The ten highest of it | | 0.037 ms | 0.006 ms |
| The ten lowest of the half that are open | | 0.041 ms | 100 ms |
| The count of the last 500 of it | 983 ms | 0.025 ms | 0.008 ms |
| A page of 50 near the end of creation order | 191 ms | 0.025 ms | 0.028 ms |
| A page of 50 near the end of the number's order | | 0.060 ms | 0.033 ms |
| A check | 34.3 s | 34.9 s | 2.3 s |
| A rebuild | 36.4 s | 36.6 s | 2.1 s |

The ten lowest of the open half visited ten entities: the notes the benchmark's commits made hold the least numbers
and are open, so the walk meets its ten at once. On a quiet machine the load, the creates, the check and the rebuild
are what they were before an index accounted for its entities, which the reading under load two sections above
couldn't say.

## With a value index's places in blocks

The benchmark once a value index holds a value's places together (`../architecture/format.md`, `d/x`), on
`session/value-blocks` at `5646e7f`, the same machine, 2026-11-11. The machine was busy with other work: load 13.3
as it began and 7.8 as it ended, where the column before it was read at 3.6 falling to 2.5, which the seconds carry.
SQLite's readings are its own of this run; its load took 43.6 s where it had taken 33.1.

| | A key an entity | Places in blocks | SQLite |
|---|---:|---:|---:|
| Bytes a note, as loaded | 1,174 | 956 | 151 |
| Bytes a note, compacted | 1,186 | 1,042 | 145 |
| Load | 57.7 s | 70.7 s | 43.6 s |
| A create of three terms every note holds, at a million | 0.167 ms | 0.150 ms | 0.056 ms |
| A create of thirty terms, fifteen widely held, at a million | 0.415 ms | 0.439 ms | 0.071 ms |
| The ten lowest of the indexed number | 0.030 ms | 0.030 ms | 0.009 ms |
| The ten highest of it | 0.037 ms | 0.031 ms | 0.007 ms |
| The ten lowest of the half that are open | 0.041 ms | 0.038 ms | 106 ms |
| The count of the last 500 of it | 0.025 ms | 0.049 ms | 0.014 ms |
| A page of 50 near the end of creation order | 0.025 ms | 0.028 ms | 0.022 ms |
| A page of 50 near the end of the number's order | 0.060 ms | 0.059 ms | 0.046 ms |
| A check | 34.9 s | 38.8 s | 2.4 s |
| A rebuild | 36.6 s | 43.0 s | 2.3 s |

What each read visited is what it was: ten, ten, ten, 500, fifty and fifty.

The 218 bytes a note are the status index's: a key for each note under one of two values became a block of 256
places. The number's index holds a key for each note as it did, every note holding a number of its own, so the two
indexes together went from about 400 bytes a note to about 180. The count of the last 500 reads 500 keys that each
hold one place, and costs twice what it did. The load, the check and the rebuild are within what the load on the
machine and SQLite's own slower load account for; none was read on a quiet machine.

Two runs before this one, on the same tree but for the rebuild, read a rebuild of 85 s each, twice the check. A
rebuild writes every derived key again in one unit, where a page it frees is no room until the unit commits, and
this database, smaller than it had been, used just under half its map: the rebuild ran until the map was full,
the map grew, and it ran again. `main` did the same at other sizes, which a disposable program through
`Database::rebuild` showed on the same day at a load of 35 to 50: 220,000 notes of a number and a status rebuilt in
10.0 s where a check took 3.9 s. A rebuild of everything now grows the map to three times what is in use before it
begins (`../architecture/engine.md`, Admin), and the same program read 3.7 s for those 220,000, at a load
of 7.

## Off the calling thread: a rebuild of 200,000 notes

`write`, `check`, `rebuild` and `backup` run on a thread of the pool (`architecture/engine.md`, Writes), so a rebuild of a
large database doesn't hold the event loop. The measurement: a disposable program through the binding, 200,000 notes of a
string and a number, each indexed, filled with 5,000-op writes, then `rebuild()` on the pool and `rebuildSync()` on the
calling thread, each on a database of its own. The longest stretch the event loop was held was read with a 1 ms timer.
Apple M1 Pro, 2026-11-12, the machine busy with other work.

| run | load | `rebuild()` on the pool | longest hold of the event loop | `rebuildSync()` on the calling thread |
|---|---:|---:|---:|---|
| 1 | 36 | 3.99 s | 5.8 ms | 4.47 s, no timer tick in it |
| 2 | 38 | 5.61 s | 11.9 ms | 6.26 s, no timer tick in it |
| 3 | 69 | not read | 16.8 ms | not read |

The timer's own worst gap on this machine with nothing else running was 4.9 ms and 5.6 ms (load 41 to 43), so the pool's
longest holds are mostly the scheduler's. The 10 ms target is not cleanly met at these loads; a quiet-machine reading is
needed to claim it. The calling-thread form held the loop for the whole rebuild each time.
