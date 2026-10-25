# Search

Text and vector indexes, and search as plan steps. Status: text search and fuzzy labels are built
(`crates/elmdb/src/engine/text/`), and vector and hybrid search (`crates/elmdb/src/engine/vector/`); substring and
regex are designed, not built.

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
and writes the vectors back in a batch, which the engine accepts only if the source hasn't changed since. `stats()`
reports how many are pending, so a search can say it is incomplete.

## Text search as built

- **Keys** (`format.md`, Keys): per type and index, a term's posting list (`d/ft`, a roaring bitmap of the
  entities' places), its positions in each entity (`d/fp`), each entity's document — id, each field's length, its
  terms — (`d/fd`), and the index's statistics (`d/fs`: documents, each field's total length). Every write that changes
  an indexed field refiles the entity from its stored document, in the same unit; an index declared over existing data
  is built in the background as a value index is, and `check()` and `rebuild()` cover it.
- **Fields**: a `string` field, a list of strings, or an `any` field, whose value is text when it is one of those.
  A list's elements are positions apart enough that no phrase spans two. A term longer than 128 bytes isn't held.
- **The query**: words, all of which match unless `any`; `"a phrase"`, adjacent in one field; `-word`; `field:word`;
  `a OR b`. The last word matches the terms it starts with `prefix`; a word of five letters or more also matches a term
  an edit away, nine or more two, unless `fuzzy: false`; the word itself outranks both.
- **Ranking**: BM25F (k1 1.2, b 0.75), each field's frequency and length counted at its weight, then a recency decay
  when asked: a score halved every `halfLife` milliseconds the field's time is before `now`. Ties keep the query's
  order.
- **As a plan step**: `search` after a type start reads the posting lists; after other steps it searches among the
  entities they reached, in which a type with no text index holds no match. `hits` answers each entity's score,
  matched terms and highlights (`[element, start, end]` in UTF-16 units) with the fields asked for; `groupBy` with
  `counts` is a facet; `after` pages by the last id of the page before.
- **Reads**: `suggest` completes a word from the dictionary, most held first, and corrects it, nearest first.
  `findLabels` is the quick-open: a query's characters in order in each type's label field, scored for word starts,
  runs and position, best first.

## Vector search as built

- **Keys** (`format.md`, Keys): an embedding is primary data (`p/q`: its model, its chunk's hash, its f32 values); each
  chunk's hash and text (`d/qc`), a pending marker per chunk waiting for one (`d/qp`), the graph item of each embedded
  chunk both ways (`d/qi`, `d/qm`), a unit's graph changes (`d/qj`) and each index's graph number (`d/qn`). The graphs
  are hannoy's, in LMDB databases of their own (`elmdb.graphs.cosine`, `elmdb.graphs.euclidean`).
- **Writes** refile a changed entity's chunks in the same unit: a chunk whose text changed loses its embedding and its
  graph item and is pending. `embed` stores a vector only for the chunk's current hash, and answers `stale` otherwise.
- **Graphs** change once per unit, as the outermost unit commits, from the journal a savepoint rolls back with
  everything else; `check()` and `rebuild()` treat them as derived from the stored embeddings.
- **Reads**: `near` (score `1 / (1 + distance)`, an entity once by its nearest chunk), `like` (an entity's own
  embedding), `hybrid` (reciprocal rank fusion of the text ranking and the vector ranking, k = 60), each among what
  the steps before it reached and ended by `hits`; `pendingEmbeddings`.

## Order of work

1. Ranked full text over messages, notes and documents, with filters and highlights, replacing apack #148.
2. Fuzzy labels for a quick-open across entities.
3. Vector and hybrid search, replacing apack's dormant library index.
4. Substring and regex, if text search leaves a gap.
