# Search

Text and vector indexes, and search as plan steps. Status: text search and fuzzy labels are built
(`crates/elmdb/src/engine/text/`), vector and hybrid search (`crates/elmdb/src/engine/vector/`), and substring and
regex search (`crates/elmdb/src/engine/trigram.rs`).

## Why in the engine

A search index kept beside a database drifts from it: it is rebuilt at boot, it misses writes after a crash, and a
backup has to copy it separately. apack has met each of these: its draft message search (apack #148, MiniSearch)
rebuilds in memory on every boot, and its dormant library search (usearch) keeps index and mapping files that the
backup screen copies on its own. In elmdb a text index commits in the same transaction as the data, and a vector index
records in that transaction what it still has to embed. Both are copied with the database and checked by `check()`.

Meilisearch is the precedent: its engine keeps its inverted index and its vector stores (arroy, hannoy) in LMDB.

## What there is to search in apack

Messages (text, sender, thread), thread titles, notes and library documents, prompts, actions, flows and their nodes,
artifacts, browser tabs and bookmarks (title, URL), flow runs (inputs, outputs, errors), and settings. Two kinds of
reader: a person at a search box or a quick-open, and an agent retrieving context for a step.

## Declaring

Text and vector indexes are named indexes on a type, beside its value indexes (`schema.md`, Indexes):

```json
"indexes": {
  "text":    { "kind": "text", "fields": { "title": { "weight": 3 }, "body": { "weight": 1 } }, "analyzer": "standard" },
  "meaning": { "kind": "vector", "from": ["title", "body"], "chunk": "paragraph",
               "dims": 384, "metric": "cosine", "precision": "f32", "model": "bge-small-en-v1.5" }
}
```

- **Text:** fields with weights, and an analyzer (charabia's tokenizing, which handles many languages; identifiers
  split on camelCase, snake_case, paths and codes like `THR-12`; case and accents folded; stemming optional).
- **Vector:** the fields it is computed from and how text is split into chunks, dimensions, metric, storage precision,
  and the model, recorded with each vector. Changing the model makes every vector pending again.

## Methods

| Method | How |
|---|---|
| Ranked full text | BM25 over postings (roaring bitmaps of entities per term, positions beside them), per-field weights, AND, OR, NOT, a field named in the query (`sender:`) |
| Phrases | positions |
| Prefix | a range over the sorted term dictionary, for search as you type |
| Fuzzy terms | a Levenshtein automaton intersected with the term dictionary |
| Substring and regex | a trigram index narrowing candidates, then a check of each, for error text, ids and code in messages |
| Fuzzy labels | subsequence scoring over labels, for a quick-open across every entity |
| Vector | hannoy (HNSW on LMDB), filtered by a bitmap of allowed entities |
| Hybrid | full text and vector merged by reciprocal rank fusion |
| More like this | an entity's own vector as the query |
| Suggestions | completing a term from the dictionary, and the nearest terms for "did you mean" |

## As plan steps

Search steps are plan steps, so everything else a plan does combines with them:

- **Filters:** type, scope through relations (inside a thread, under a folder's descendants), field values, roles,
  time ranges, trash excluded unless asked for. A filter becomes a bitmap the search runs inside.
- **Order:** relevance, a field, or relevance with a recency decay.
- **Facets:** counts per type, per field value or per related entity, over the matches.
- **Paging:** top k, and a cursor after the last result.
- **Results:** scores, the matched terms, and highlight offsets per field.
- **Watch:** a watched search runs again when a commit touches its fields.

Reranking with a model is the caller's: apack has `rerank` in `services.inference`, applied to the engine's top k.

## Embeddings

The engine never calls a model. A write that changes a vector index's source fields records the entity's chunks as
pending, in the same unit. A caller's worker takes pending chunks (`db.pendingEmbeddings(index, limit)`), embeds them
and writes the vectors back in a batch, which the engine accepts only if the source hasn't changed since, and answers
`stale` for one whose entity is gone. `pendingEmbeddings` is a read (`ReadOp`), so a caller asks it within the same
snapshot as a search when it needs to say that search is incomplete.

## Text search as built

- **Keys** (`format.md`, Keys): per type and index, a term's posting list (`d/ft`, a roaring bitmap of the
  entities' places), its positions in each entity (`d/fp`), each entity's document — id, each field's length, its
  terms and words as written — (`d/fd`), the index's statistics (`d/fs`: documents, each field's total length), and,
  in a stemming index, each word as written and the entities holding it (`d/fw`). Every write that changes an indexed
  field refiles the entity from its stored document, in the same unit; an index declared over existing data is built
  in the background as a value index is, and `check()` and `rebuild()` cover it.
- **Fields**: a `string` field, a list of strings, or an `any` field, whose value is text when it is one of those.
  A list's elements are positions apart enough that no phrase spans two. A term longer than 128 bytes isn't held.
- **The query**: words, all of which match unless `any`; `"a phrase"`, adjacent in one field; `-word`; `field:word`;
  `a OR b`. The last word matches the terms it starts with `prefix`, and in a stemming index the stems of the words as
  written it starts, since a stem need not start as its word does (`runni` starts `running`, held as `run`); a word of
  five letters or more also matches a term an edit away, nine or more two, unless `fuzzy: false`; the word itself
  outranks both. A fuzzy word's automaton walks the sorted dictionary a byte at a time, and where no term starting
  with the bytes read can match it seeks past them all, so a dictionary of names sharing their first letters (ids,
  paths) is skipped rather than read (`docs/evidence/next-phase-6.md`).
- **Ranking**: BM25F (k1 1.2, b 0.75), each field's frequency and length counted at its weight, then a recency decay
  when asked: a score halved every `halfLife` milliseconds the field's time is before `now`. Ties keep the query's
  order.
- **As a plan step**: `search` after a type start reads the posting lists; after other steps it searches among the
  entities they reached, in which a type with no text index holds no match. `hits` answers each entity's score,
  matched terms and highlights (`[element, start, end]` in UTF-16 units) with the fields asked for; `groupBy` with
  `counts` is a facet; `after` pages by the last id of the page before.
- **Reads**: `suggest` completes a word from the dictionary, most held first, and corrects it, nearest first; a
  stemming index answers from its words as written, so a suggestion is a word (`database`) and never a stem
  (`databas`).
  `findLabels` is the quick-open: a query's characters in order in each type's label field, scored for word starts,
  runs and position, best first.

## Substrings and patterns as built

- **Keys** (`format.md`, Keys): per type and index, each run of three characters an entity's text holds, case folded
  (each character lowercased on its own), with the places of the entities holding it as a roaring bitmap, one key per
  block of 2^16 places (`d/g trigram block`). A place only grows, so a new entity changes its trigrams' last blocks. A
  write refiles what the changed field's text gained and lost, in the same unit; an index declared over existing data
  is built in the background as a value index is, and `check()` and `rebuild()` cover it. On G8's data a key per
  trigram and entity tripled the file, and blocks add an eighth (`docs/evidence/next-phase-9.md`).
- **Fields**: as a text index's. Each string, and each element of a list, is apart, so no run spans two.
- **`containing`** a text, exactly or with `caseInsensitive`; **`matching`** a pattern in Rust's `regex` syntax (G11 of
  `goal-elmdb-next.md`), which has no look-around or backreferences: a pattern using them, or one that doesn't parse,
  is refused (`PATTERN_INVALID`) with the parser's reason.
- **Narrowing**: what every match holds is read off the text, or off the pattern's syntax — its literals, all of a
  sequence's parts, one of an alternation's branches, a repetition's body when it repeats at least once — and the
  places of the entities holding each run of it are intersected, or for an alternation joined. Each candidate is then
  checked against its text, so a search answers exactly what checking every entity would; what can't narrow (a text
  under three characters, a class, a branch that narrows by nothing, an index not yet built) reads every entity of the
  type and checks each.
- **As a plan step**: after a type start it reads the candidates in place order, which is the order a walk of the type
  answers; after other steps it filters what they reached, in their order, in which a type with no trigram index holds
  no match. It doesn't rank: a search box that wants both orders a `search` and a `containing` together.

## Vector search as built

- **Keys** (`format.md`, Keys): an embedding is primary data (`p/q`: its model, its chunk's hash, its f32 values); each
  chunk's hash and text (`d/qc`), a pending marker per chunk waiting for one (`d/qp`), the graph item of each embedded
  chunk both ways (`d/qi`, `d/qm`), a unit's graph changes (`d/qj`) and each index's graph number (`d/qn`). The graphs
  are hannoy's, in LMDB databases of their own (`elmdb.graphs.cosine`, `elmdb.graphs.euclidean`).
- **Writes** refile a changed entity's chunks in the same unit: a chunk whose text changed loses its embedding and its
  graph item and is pending. `embed` stores a vector only for the chunk's current hash, and answers `stale` otherwise.
- **Graphs** change once per unit, as the outermost unit commits, from the journal a savepoint rolls back with
  everything else; `check()` and `rebuild()` treat them as derived from the stored embeddings.
  Reads within a unit omit journaled items from the committed graph results and merge the current primary
  embeddings for those items, using the graph's distance metric. New embeddings are visible even before the
  first graph is built. Savepoint rollback restores this read view with the journal and primary data; a read
  never flushes graph writes outside the savepoint undo log. Additional scoring is proportional to that
  index's pending changes in the query scope, rather than every stored embedding.
- **Reads**: `near` (score `1 / (1 + distance)`, an entity once by its nearest chunk), `like` (an entity's own
  embedding), `hybrid` (reciprocal rank fusion of the text ranking and the vector ranking, k = 60), each among what
  the steps before it reached and ended by `hits`; `pendingEmbeddings`.

## Order of work

1. Ranked full text over messages, notes and documents, with filters and highlights, replacing apack #148.
2. Fuzzy labels for a quick-open across entities.
3. Vector and hybrid search, replacing apack's dormant library index.
4. Substring and regex, for what words don't find: error text, ids, hashes and code (`docs/evidence/next-phase-9.md`).
