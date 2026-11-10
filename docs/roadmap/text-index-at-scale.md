# The text index at scale

Status: designed again on 2026-11-10 from readings at a million notes (`../evidence/million-notes.md`); nothing of the
design below is built. Fuzzy terms by seeking is built (2026-10-27). Posting lists in blocks of a fixed range of places
were built and measured at 50,000 messages, changed no write's time there and were not kept
(`../evidence/next-phase-6.md`); at a million notes the rewrite of whole lists is most of a commit, which this design
answers. It is ordered by the road to 1.0 (`../roadmap.md`), and is built before the format is frozen.

## Where it starts

A text index (`../architecture/search.md`; the layout in `crates/elmdb/src/engine/text/mod.rs`) keeps one roaring
bitmap per term, positions per term and entity, a document record per entity and the index's statistics, all written
by the unit that changes them. At a million notes, four costs have grown with the notes and not with the change:

- **Writes.** A create holding thirty terms that exist, fifteen of them widely held, commits in 1.3 ms, where a create
  in a type with no text index commits in 0.06 ms. Each widely held term's list is read, decoded, extended, encoded and
  written again, 75 µs a term, and the commit writes about 211 pages.
- **Tokenizing** is done by the commit: 10.5 µs for a message of thirty words and 138 µs for a reply of 2.6 KB. A reply
  streamed in 181 writes (`../evidence/next-phase-10.md`) is tokenized 181 times.
- **Ranking.** The best ten for a word every note holds take 3.6 s, and its count as long. Every match is scored, and
  scoring one reads three keys: its document record, the term's positions in it, and its entity's record.
- **Bytes.** Positions are a key per term and entity, 178 MB of keys for 27 MB of values, and the pages of the file are
  half full.

## The design

What stays: a search answers from the data of its own transaction. A unit that writes a note and then searches finds
it, and so does every read after the commit, with the score it would have had were the index written by the commit.
What changes is when the index's keys are written, which today is in the unit (`../architecture/design.md`,
Principles) and becomes after it, as a rank index's already are.

1. **A commit notes the place.** A write that changes an entity's text in an index adds its place to the index's
   journal, `d/fj type index`, and writes nothing else of the index: no term is read and no text is tokenized. A vector
   index has such a journal (`d/qj`), emptied as its unit commits.
2. **One routine files a place.** `text::file` already writes the difference between the document as stored and the
   entity's text now. The builder calls it for journaled places, in transactions bounded in places and in time. A unit
   calls it itself for its own places when they are many, and when the journal is full, so the journal has a bound
   and a search's work beside the index has one too. A unit of a thousand messages gains nothing from filing in term
   order, so there is no merge and no order to keep.
3. **A term's places are held in blocks, each place with the term's count in each field.** `d/ft type index term`
   holds how many entities hold the term and the block still open; a block that reaches about 2 KB is sealed under
   `d/ft type index term place`, the first place in it. Adding a place rewrites one block, whatever the term holds. An
   entry is the place's distance from the one before it and the count for each field holding the term, which is what
   BM25F reads (`crates/elmdb/src/engine/text/search.rs`). A stemming index's words as written (`d/fw`) are held the
   same way.
4. **Lengths are held by place**, `d/fl type index field block`, a block of 1,024 places. Positions move into the
   document record (`d/fd`), which a phrase's candidates read and no other match does, and `d/fp` goes.
5. **A search reads the journal first.** The places in it are taken out of every term's places and matched against
   their entities' text as it is, tokenized then, and the statistics are corrected by the same difference. The answer,
   each score included, is then the same whether or not a place has been filed, which is what the model harness holds
   a search to.
6. **Ranking reads counts and lengths.** Scoring a match reads no key of its own. Hits are handed to the plan best
   first, and a hit's document record, for its id and the terms and fields it matched, is read when the plan visits
   it. The type's trashed places (`d/t`) are taken out as a set before ranking, where a plan reads each hit's entity
   record today. A count scores nothing.
7. **A tracked search reads the journal's key**, so it hears every commit that changes text in its index, as one with
   a match hears it today through the statistics' key. A commit that files changes no answer and names no tracked
   read.

### Beyond text

- **A trigram index** takes the same journal, and keeps its layout: its places are already in blocks.
- **A vector index's journal outlives its commit**, and the builder applies it to the graph: change 6 of the road to
  1.0. Of a vector index's bytes, the chunk's text in `d/qc` is the entity's own text a second time, 8.9 MB of a
  124.6 MB file of 20,000 vectors: whether a read can chunk the field again in its place is read first. The graph's
  own copy of every vector, 29.4 MB there, is hannoy's to shrink by a quantized distance, whose recall is unmeasured.
- **A family of keys that only appends fills its pages only where it ends a tree**, since LMDB splits a page in the
  middle unless the new key goes at its end. Two such families written in turn into one database leave its pages 65%
  full, and 98% full with a database each. The document records, the lengths and a type's entities in creation order
  are such families.

## Readings

On LMDB alone, with the layouts above written by a disposable program: a million documents, one text field, a message
of thirty terms of which fifteen are widely held (`../evidence/million-notes.md`, The layouts, for the conditions).

| | Lists whole, as today | Blocks, written by the commit | A note in the journal |
|---|---:|---:|---:|
| A commit | 0.83 ms | 0.15 ms | 0.011 ms |
| The pages it writes | 211 | 45 | 5 |
| A message in a unit of 1,000 | 0.44 ms | 0.055 ms | 0.055 ms, the unit filing its own |
| The index | 372 MB | 157 MB | 157 MB |

The best ten for a term every document holds, read from the blocks and the lengths: 2.8 ms, 3 ns a match.

## What was weighed

- *Blocks of a fixed range of places*, a roaring bitmap each (`term | seq >> 16`), as a trigram index has: they bound a
  write the same way, and were built once. They carry no counts, so ranking would still read a key a match.
- *One key per term and entity* (`term | seq → frequency`): no rewrite, but a common term's places become a million
  keys to scan, and every key repeats the type, the index and the term. `d/fp` is this shape and is most of the index.
- *The commit writing the document's terms, the builder merging them into the terms' blocks later*: 0.014 to 0.025 ms
  and 7 to 9 pages a commit, but the commit still tokenizes, a merge of 10,000 messages held the writer for 292 ms, and
  nothing bounds what waits.
- *Filing a unit's places in term order*: 46 to 53 ms for a unit of 1,000 messages against 54 to 56 ms in the order
  written, and more pages. One transaction is what makes a unit cheap.
- *Segments with background merges*, as Lucene and Meilisearch do: bounded writes too, at the cost of a merge policy
  and of derived data that is correct in more than one layout. Not needed where the store is ordered.

## Order, each with its gate

Each is read from the benchmark against SQLite (`../architecture/testing.md`), at a million notes.

1. **Blocks, lengths and ranking** (3, 4 and 6 above). The best ten for a word every note holds visit about ten
   entities and take under 10 ms; a count scores nothing; the index's bytes fall by half.
2. **The journal, for text and trigram indexes** (1, 2, 5 and 7). A create's commit costs at a million notes what it
   does at 50,000, and the same for a long text as for a short one; the model harness shows a search's answer the same
   before a place is filed and after.
3. **Vectors on the journal.** Change 6's own gate.
4. **A database for each family that only appends.** Their leaves are nine tenths full.

Block-max scores, which skip a block that can't reach the best ten, wait for a reading that calls for them: a block's
header keeps room for its greatest score.

## Open questions

- **A block's size against a page's.** The readings are on 16 KB pages. On 4 KB pages a 2 KB value is past what a page
  holds beside another and goes to a page of its own, so the size is read on Linux before it is chosen.
- **The journal's bound.** What a search reads beside the index is the journaled text, 55 µs a KB to tokenize, so the
  bound is on the text that waits and not on the places.
- **`recency`** reads a field of every match (`search.rs`). It needs the field held by place as a length is, which the
  aggregates of milestone 0.2 would read too.
- **A filing commit and tracked reads.** It rewrites keys a tracked search read and changes no answer; how the commit
  path knows to name no token is designed with the journal.
- **The derived layout's version.** Kept apart from the primary format's, a later change to it is a rebuild at open
  and not a migration, and the freeze covers what can't be rebuilt.
- **Primary values** spend 82 MB of keys on 52 MB of values at a million notes, the most bytes left. A key per field
  is what lets a tracked read name one field, so it is a design of its own, decided before the freeze.
