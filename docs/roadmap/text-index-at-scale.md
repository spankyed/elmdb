# The text index at scale

Status: fuzzy terms by seeking built, 2026-10-27, its reading calling for it. Posting lists in blocks were built and measured at 4,096 and 1,024 places and changed no write's time — a write's growth is mostly the store's own, a message with no text growing alike — so they are not built (`../evidence/next-phase-6.md`). Block-max scores: a common word's ranking is measured, left to its trigger.

## Where it starts

A text index (`../architecture/search.md`; the layout in `crates/elmdb/src/engine/text/mod.rs`) keeps one roaring
bitmap per term, positions per term and entity, a document record per entity and the index's statistics, all
rewritten by the write that changes them. Three costs grow with the corpus rather than with the change:

- **Writes.** Adding a document rewrites the whole posting list of every term it holds, so a common word's list — `the`
  in a chat log — is read, decoded, extended, encoded and written again for every new message. This is the one most
  likely to show first: a chat log only grows.
- **Fuzzy terms.** A fuzzy word runs its Levenshtein automaton over every term of the index.
- **Ranking.** Every matching document is scored before the top results are kept: a common word scores every document
  holding it.

apack's corpus is small today, and none of these is measured as a problem yet.

## The idea: use the B-tree LMDB already is

Lucene's segments and background merges, and a separate FST dictionary, exist for stores without an ordered index.
LMDB is an ordered B-tree, so each cost has a smaller answer.

- **Posting lists in blocks.** A term's postings are one key per term and block of places — `term | seq >> 16` — each
  holding a roaring bitmap of the places in that block. An entity's place only grows, so a new message touches only its
  terms' last blocks: a write's rewrite is bounded by the block, and there is nothing to merge. A read decodes a term's
  few blocks, and intersecting two terms is still a bitmap AND, block by block.
- **Term frequencies stay where they are.** Positions are already one key per term and entity (`d/fp`); scoring reads a
  frequency from there, and a phrase or a highlight its positions.
- **Fuzzy terms by seeking.** The automaton walks the sorted term keys, seeking past every range it can't match, as
  Lucene's term dictionary is walked — no FST beside the B-tree.
- **Ranked top-k** keeps a heap of the best k as documents are scored; skipping blocks that can't reach it (block-max
  WAND) needs a maximum score per block, worth adding only when a common word's search is measured slow.

**The other designs weighed:**

- *One key per term and entity* (`term | seq → frequency`) also removes the rewrite, with an O(log n) insert per term.
  But a common term's postings become thousands of keys to scan rather than a few bitmaps to decode, intersecting two
  terms walks both ranges, and every entry repeats the type, index and term in its key. Blocks keep the bounded write
  and the bitmap reads.
- *Segments with background merges*, as Lucene and Meilisearch do: bounded writes too, at the cost of a merge policy,
  a merge thread and derived data that is correct in more than one layout. Not needed where the store is ordered.

## When

Each part has its own trigger, read off a text case in apack's `npm run bench` and off production-shaped data:

- blocks, when a message write's commit time grows with the corpus;
- seeking for fuzzy terms, when a fuzzy search's time grows with the dictionary;
- block-max scores, when a common word's search time does.

## Open questions

- The block size: `2^16` places keeps a block's bitmap small and a common term's blocks few; measured before choosing.
- The format change: a text index's keys change shape, which `../architecture/format.md`'s versioning carries, and an
  existing index is rebuilt.
