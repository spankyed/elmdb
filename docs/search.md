# Search

Text and vector indexes, and search as plan steps. Status: designed, not built.

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
               "dims": 384, "metric": "cosine", "precision": "f16", "model": "bge-small-en-v1.5" }
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

## Order of work

1. Ranked full text over messages, notes and documents, with filters and highlights, replacing apack #148.
2. Fuzzy labels for a quick-open across entities.
3. Vector and hybrid search, replacing apack's dormant library index.
4. Substring and regex, if text search leaves a gap.
