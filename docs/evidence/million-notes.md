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

What these don't show: the engine with any of it built, pages of 4 KB, text with a natural spread of terms, or more
than one text field.
