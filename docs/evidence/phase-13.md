# Phase 13: vector and hybrid search

2026-10-25, MacBook Pro (Apple silicon), macOS 25.3, Node 23.11.1, Rust 1.99.1. elmdb `90207fd`; apack's worktree
`~/Develop/Projects/AgentBuddy-worktrees/ears-on-elmdb`, branch `session/ears-on-elmdb`, pinning elmdb `4a1032a`.

## In elmdb

`crates/elmdb/src/engine/vector/` (`search.md`, Embeddings):

- **Chunks and pending** (`mod.rs`): every write to a vector index's `from` fields refiles the entity in the same unit:
  its text split `whole`, at blank lines (`paragraph`) or at Markdown headings (`heading`); each chunk's text and its
  sha-256 kept (`d/qc`), a chunk with no embedding of its current text under the index's model marked pending
  (`d/qp`). A chunk whose text changed or is gone loses its embedding and its graph item. `pendingEmbeddings` answers
  the index's model, dimensions, how many wait, and up to a limit of them with their hashes and texts.
- **Embeddings** (`embed` op): primary data (`p/q`: model, hash, f32 bytes), stored only when the hash is the chunk's
  current one — otherwise answered `stale` and not stored — and refused for another model, another size or an
  unknown index.
- **Graphs**: hannoy 0.3's HNSW graphs in LMDB databases of their own (`elmdb.graphs.cosine`,
  `elmdb.graphs.euclidean`), derived from the stored embeddings. A unit's additions and removals go to a journal
  (`d/qj`) that a savepoint rolls back with everything else, and are applied to the graph once, as the outermost unit
  commits, with a seeded build. `check()` holds every item and graph to the stored embeddings, `rebuild()` builds the
  graphs again from them, and an index dropped takes its embeddings and graph with it.
- **Reads**: `near{vector,k}` (score `1 / (1 + distance)`, each entity once by its nearest chunk, among the entities the
  steps before it reached), `like{id,k}` (an entity's own embedding, itself left out) and `hybrid{query,vector,k}`
  (the text ranking and the vector ranking merged by reciprocal rank fusion, k = 60). `hits` ends each.
- **D29**: a vector index's metric is `cosine` or `l2` and its precision `f32`, which is what hannoy measures.

**Checks.** `cargo test --workspace` passes, among them the vector specs' 12: chunking; pending until embedded and
again when the text changes; refusals of another model, size or index; `near` within earlier steps and without the
trash; `like`; `hybrid` on a fixed corpus; a unit changing the graphs as it commits and a rolled-back one not at all;
`rebuild()` keeping the embeddings; an index declared over existing data and a dropped one; a reset. The binding's 18
tests, one embedding pending chunks and searching through it from JavaScript, pass. Clippy and fmt are clean.

## In apack

- **The manifest**: an entity entry's `embed: { from, chunk?, model, dims }`, `model` a `provider:model` from a
  provider that embeds, `from` checked against the shape's fields by codegen as `search`'s are; `RowRules.embed`
  becomes an elmdb vector index named `embedding`, cosine over f32.
- **The engine** (`@apackdev/ears`): `pendingEmbeddings(type, { limit })`, `embed(embeddings)` answering
  `{ stored, stale }`, `embeddedTypes()`; the builder's `near(vector, { k })`, `like(id, { k })` and
  `hybrid(text, vector, { k, ...searchOptions })`, each ended by `hits()`. `apack-ears/tests/engine/vector.spec.ts`
  (5 cases): pending until embedded and pending again when a read field changes, and not when another does; a stale
  vector refused and its chunk pending under its new text; `near` within earlier steps and `like`; hybrid on a fixed
  corpus — every note in the vector ranking, the boat's first, three titled "stars" in the text ranking — merging to the
  three both rankings hold above the boat, which `k: 3` leaves out; the refusals.
- **The embedding worker** (`@apackdev/sdk/services`): `embedPending({ types?, batch? })` takes each index's pending
  chunks, embeds them with `services.inference.embedMany` under the index's model, and stores them, until none waits;
  a chunk changed while its batch was in flight is stale, and embedded again. `embedSearchText(type, text)` makes a
  search's vector with the index's model. The engine never calls a model, and nothing embeds on a timer: whoever
  writes the text runs it after (D31). `apack-sdk/tests/services/embeddings.spec.ts` (3 cases), through
  `fakeInference`: two types' chunks embedded each with its own model, found by `near`; a changed paragraph pending
  while the unchanged one keeps its embedding, embedded again; the in-flight change counted stale and re-embedded.
- **The library port** (D31): the dormant library search index keeps its `SearchIndex` rows and segment rules, and each
  chunk is an `IndexedDoc` row whose `text` IndexedDoc's vector index embeds (`openai:text-embedding-3-small`, 1536).
  `fastembed`, `usearch`, the local FastEmbed models, the index files under the data directory, the per-index model,
  metric and connectivity, and the editor's controls for them are gone; the module is typechecked again (out of
  default-setup's `tsconfig.json` exclusions) and stays off (D15), its call sites still commented. Indexing writes
  rows; `embedIndexedDocs()` embeds; `searchInIndex` ranks the index's rows by `near`.
  `default-setup/tests/features/library/be/search-index.spec.ts` (4 cases): a folder and its subfolder indexed, pending,
  embedded with IndexedDoc's model and searched; an excluded subfolder and an outside document left out; an edited
  document's chunk pending and unfound, then found by its new text once embedded; a list split one chunk per item.
- **Typed contract** (`TYPED-EARS.md`): `facade-typing` gained `hybrid`'s `fields` position, passing under both module
  resolutions. API reports, the editor schema and the facade report are regenerated.

**Mutation checks**, each restored: dropping `near` from the steps `hits()` accepts fails the engine spec; not counting
stale embeddings fails the worker spec; keeping the chunks a document no longer makes fails the library's list case.
In elmdb: removing `Unit::commit`'s flush fails the unit spec, and removing `put_field`'s vector refile fails 7 specs.

**The gate.** apack's `npm run chain -- --no-native` passed on the Phase 13 tree (30 steps run, 1,337.5s,
2026-10-25), committed as `794249720`. Two runs before it failed on what they found: one because the binding package
was edited while the pack compiler had it captured (`Compiler resolution namespace changed during discovery`), which
was the run's operator and not the code; the next on a module-scope `Set` in `query.ts` that
`no-module-state.spec.ts` refuses, made an array.
