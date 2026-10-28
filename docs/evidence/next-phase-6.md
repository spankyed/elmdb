# Phase 6 of goal-elmdb-next: the text index at scale (gated)

2026-10-27. apack's `packages/apack-ears/bench/text.bench.ts` (apack `2df2961774`), in the built engine, synced as the
app's is, on Apple M1 Pro, 10 cores, 32 GiB, macOS 26.3.1, Node 23.11.1. The machine was shared with another project's
test runs throughout (load averages from 6 to 75); each figure below is from a run at a one-minute load under 14 unless
it says otherwise, and every decision rests on at least two runs agreeing.

## The reading

Three chat logs, of 1,000, 10,000 and 50,000 messages, each on a database of its own with a text index on the message's
text: thirty words drawn by a Zipf-like rank from 5,000 (a few common words in nearly every message), and two names of
the message's own, as a chat about code holds ids and paths, so the dictionary grows with the log (about 105,000 terms
at 50,000). Medians, µs:

| Case | 1,000 | 10,000 | 50,000 | 50k ÷ 1k |
|---|---:|---:|---:|---:|
| A message write | 256 | 414 | 583 | 2.27 |
| A fuzzy search (a rare word misspelled) | 1,763 | 4,215 | 16,684 | 9.47 |
| A common word misspelled | 2,895 | 8,920 | 37,377 | 12.91 |
| A common word searched exactly | 1,202 | 4,369 | 20,215 | 16.82 |

A second run agreed within 4% on every ratio (2.26, 9.76, 13.11, 17.65).

**The fuzzy reading had to be corrected first.** The gate read "a fuzzy search's time", and the first fuzzy case
searched a misspelled *common* word: 11× at 50,000, but the same word searched exactly grew 17×, so what grew was
ranking its thousands of matches, not the dictionary walk the gate is about. With a vocabulary capped at 5,000 the
dictionary didn't grow at all, so a rare word's fuzzy search (1.37×) measured nothing either. The bench now grows the
dictionary with the log, and the gate reads a rare word's fuzzy search, which walks the dictionary and ranks few; the
common word's growth is ranking's, block-max scores' trigger (`../roadmap/text-index-at-scale.md`, When). The goal's
G5 records the change.

## Fuzzy terms by seeking: built

The gate fired (9.47× > 2). elmdb `e4ff088`: the automaton reads each term a byte at a time, and where no term starting
with the bytes read can match, the walk seeks past all of them (`Read::visit_from`), rather than evaluating every term
(`crates/elmdb/src/engine/text/search.rs`, `within`; `../architecture/search.md`, The query).

| Case, after (µs) | 1,000 | 10,000 | 50,000 | 50k ÷ 1k |
|---|---:|---:|---:|---:|
| A fuzzy search (a rare word misspelled) | 851 | 988 | 1,239 | 1.46 |
| A common word misspelled | 2,020 | 5,413 | 20,969 | 10.38 |
| A common word searched exactly | 1,192 | 4,301 | 20,282 | 17.01 |

A second run agreed (1.46, 10.98, 17.73). The rare word's search at 50,000: 16.7 ms to 1.24 ms. A common word misspelled
lost its dictionary walk and is now its exact search's ranking plus its neighbours'.

The spec (`search.rs`, `seeking`): over a dictionary of 3,000 words and 20,000 ids sharing their first letters, 200
queries at one and two edits find exactly what evaluating every term finds, reading under a tenth of the entries. Beside
each query the fixture holds a term the walk gives up on at its third byte and, one byte value above it there, a term an
edit away, which a seek too far would skip. Two disposable mutations on a copy-on-write clone each fail it: a walk that
never seeks (the read count), and a seek one byte too far (`hpbeb within 1` loses its match). The first version of the
fixture, random words from six and then twenty letters, let the second mutation survive and the walk read 25% and 47%
of the dictionary: words that share no prefixes are not what seeking is for, and a chat's ids are.

## Posting lists in blocks: measured, not built

The gate fired too (2.27× > 2). Blocks were built — a term's and a word's postings a bitmap per block of places under
its key, a write touching only its own block's, `check()` and `rebuild()` deriving the same — and measured at two sizes:

| A message write (µs) | 1,000 | 10,000 | 50,000 | 50k ÷ 1k |
|---|---:|---:|---:|---:|
| One bitmap per term | 256 | 414 | 583 | 2.27 |
| Blocks of 4,096 places | 268, 265 | 439, 427 | 556, 580 | 2.08, 2.19 |
| Blocks of 1,024 places | 260, 271 | 435, 445 | 570, 560 | 2.19, 2.07 |

Neither moved the write. A probe found why: a message with no text grew 2.15× on its own (40, 58, 86 µs), so most of a
write's growth is the store's, as the database grows, which no layout of the text index changes; and a message of thirty
new words grew 1.46× (141, 151, 205 µs). The text index's share grows with the corpus, but not because a common term's
bitmap grows: bounding every bitmap to 1,024 places left it where it was. Blocks would add a layout change and a union
on every read for nothing measured, so they were reverted, and the roadmap keeps them as an idea whose cause isn't yet
the one a reading shows. What does grow in a write is for a profile to say (`../roadmap/verification-tooling.md`, stage
6); the bench keeps the message with no text, so a later reading can tell the store's growth from the index's.

## Ranking: recorded

A common word's exact search grew 17×: every matching document is scored before the top twenty are kept. That is
block-max scores' trigger, which Phase 6 doesn't build; the reading is the design's input in Phase 10.

## G4

No engine path but the fuzzy walk changed. apack's `npm run bench` (`ears.bench.ts`) holds no fuzzy case, and its other
cases were read at Phase 4 over the same reads and writes. The single-row read through the binding takes no text index.

## Along the way

A unit of 50,000 messages with their postings outgrows LMDB's map (`MDB_MAP_FULL`): the map can't grow while a
transaction is open, so a load that large is written in units of a thousand.
