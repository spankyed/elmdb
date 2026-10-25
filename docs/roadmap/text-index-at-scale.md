# The text index at scale

Status: idea; built when a reading says it is needed.

## Where it starts

A text index (`../architecture/search.md`; the layout in `crates/elmdb/src/engine/text/mod.rs`) keeps one roaring
bitmap per term, positions per term and entity, a document record per entity and the index's statistics, all
rewritten by the write that changes them. Three costs grow with the corpus rather than with the change:

- **Writes.** Adding a document rewrites the whole posting list of every term it holds, so a common word's list — `the`
  in a chat log — is read, decoded, extended, encoded and written again for every new message, and the statistics key
  is rewritten by every write.
- **Fuzzy and prefix terms.** A fuzzy word runs its Levenshtein automaton over every term of the index, and a prefix is
  a range scan cut to the most-held `MAX_PREFIX_TERMS`.
- **Ranking.** Every matching document is scored, each term's frequency read from its positions key, before the top
  results are kept: a common word scores every document holding it.

apack's corpus is small today, and none of these is measured as a problem yet.

## The idea

The design Lucene and Meilisearch settled on, kept inside the unit that writes the data:

- **Segmented postings.** A write appends a small segment — the terms of the documents it changed, with each one's term
  frequency — and a background merge folds segments together, as the index builder works today, off the write's path.
  A read unions a term's segments; a deleted or changed document is masked by a per-segment deletion bitmap until a
  merge drops it. A write's cost is its own documents' terms, whatever the corpus holds.
- **Term frequencies in the postings**, beside each document's place, so scoring reads no positions; positions are read
  only for a phrase or a highlight.
- **An FST dictionary** per segment (the `fst` crate, which charabia already pulls in), intersected with the
  Levenshtein automaton and walked for a prefix, so a fuzzy word visits the terms the automaton can reach rather than
  all of them.
- **Block-max WAND** for ranked top-k: each posting block records its highest possible score, and blocks that can't
  reach the current k-th score are skipped unscored.

## When

Each part has its own trigger, read off `npm run bench` in apack with a text case added, and off production-shaped data
(apack's converted copy):

- segments, when a message write's commit time grows with the corpus;
- the FST, when a fuzzy search's time grows with the dictionary;
- WAND, when a common word's search time does.

## Open questions

- Merge policy: segment sizes, when a merge runs, and its write transaction's length against a unit the app holds open.
- `check()` and `rebuild()` over segments: derived data that is correct in more than one layout, compared after a
  canonical merge.
- The format change: a text index's keys change shape, which `format.md`'s versioning has to carry.
