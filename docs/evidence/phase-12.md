# Phase 12: text search

2026-10-25, MacBook Pro (Apple silicon), macOS 25.3, Node 23.11.1, Rust 1.99.1. elmdb `c1a1cb1`; apack's worktree
`~/Develop/Projects/AgentBuddy-worktrees/ears-on-elmdb`, branch `session/ears-on-elmdb`.

## In elmdb

`crates/elmdb/src/engine/text/` (`search.md`, Text search as built):

- **The index** (`mod.rs`): per type and text index, a term's posting list (`d/ft`, a roaring treemap of entity
  places), its positions per entity (`d/fp`), each entity's document (`d/fd`: id, field lengths, terms) and the
  statistics (`d/fs`). Every field write and delete refiles the entity from its stored document in the same unit, and a
  destroy takes it out. An index declared over existing data is built in the background like a value index;
  `check()` derives every text key from primary data and compares, `rebuild()` writes them, and a dropped index's keys
  go with its marker. A term over 128 bytes isn't held: the model harness found a 2,100-digit word failing the write
  with `MDB_BAD_VALSIZE`, LMDB's key limit.
- **The analyzer** (`analyze.rs`, D27): charabia with only its Latin camelCase and snake_case segmenters, folding case
  and accents; a code such as `THR-12` kept whole beside its parts, at its first part's place; Snowball stems when the
  index names a language. `exact` is the whole value, case folded.
- **Search** (`search.rs`): the query's words (all, or any), phrases, negations, `field:` restrictions and `OR`; each
  word expanded to the index's terms — itself, the terms it starts (the last word, with `prefix`, the 64 most held), and
  those one edit away from five letters and two from nine (levenshtein_automata DFAs, built once) — matched through the
  posting lists and checked against positions for a phrase or a field; ranked by BM25F with field weights, and a
  recency half-life when asked. `hits` answers scores, matched terms and UTF-16 highlights; `suggest` completions and
  corrections; `after` pages; `groupBy` with `counts` is a facet.
- **Fuzzy labels** (`labels.rs`, `findLabels`): a quick-open's characters in order in each type's label field, scored
  for word starts, runs and position.
- **D26**: a text index reads `any` fields as well as string ones.

**Checks.** `cargo test --workspace`: 150 pass, among them the text specs' 22 (ranking against fixed corpora — length
normalization, term frequency, a rare term over a common one, field weights, recency; the query syntax; prefix and
fuzzy terms, the word itself outranking its expansions; identifiers, codes and accents; stemming; hits and UTF-16
highlights; suggestions; labels; paging and facets; refusals; writes, an aborted unit, `check()` and `rebuild()`; a
background build and a drop). **The model harness** (`engine/tests/model.rs`) gives `Note` a text index over `a` and `b`
and compares the match sets of four words with the model's after every step of 96 random workloads, refused commits
included; deleting the refile from `delete_field` fails it, a deleted field left matching. The crash harness passes
with the index declared. The binding's 17 tests, one searching through it from JavaScript, pass. CI run 40184627225.

## In apack

- **The manifest**: an entity entry's `search: { fields: { field: weight }, stem? }`, checked against the shape's
  fields by codegen (as `indexes` is) and held to elmdb's names (D25); `RowRules.search` becomes an elmdb text index
  named `search`, declaring each field `any` that no rule declares (D26).
- **The builder**: `search(text, options?)`, `after(id)` and `hits({ fields?, highlights? })`, typed against the
  shape — `fields`, `recency.field` and `hits`' `fields` complete the shape's fields. `hits` refuses a query with no
  search, or with a `where` given a function after it (`NO_SEARCH`); a type with no text index refuses a search
  (`UNKNOWN_INDEX`). `apack-ears/tests/engine/search.spec.ts` (6 cases).
- **Text indexes**: Message (`text`), Note (`title` at 3, `content`) and, as the SDK's, Document (`name` at 3, `tags`;
  D28).
- **The consumer**: the threads panel (the chat's thread sidebar) has a search box. It sends `SEARCH_MESSAGES`, which
  the threads system answers to the asking window (`MESSAGES_FOUND`, keyed by its query) with the matches, most
  relevant first and halved for each week older, each with its thread, a snippet around its first match and the
  match marks; a compaction marker is left out. Choosing one opens its thread and, paging older messages in until it
  is loaded, scrolls it into view (`focusMessageId`). `default-setup/tests/features/threads/be/message-search.spec.ts`
  (4 cases).
- **Typed contract** (`TYPED-EARS.md`'s checklist): `facade-typing` gained the three field positions; widening
  `search`'s options to plain strings fails it under both module resolutions. API reports, the editor schema and the
  facade report are regenerated.

**The driving script** `drive/message-search.ts` (`npm run dev:drive -- drive/message-search.ts`) writes two messages as
the chat does, types `deplo` into the panel's box, and reports
`{"query":"deplo","results":[{"snippet":"Retrying the deploy after fixing the migration script","marks":[[13,19]],…},
{"snippet":"The nightly deploy failed on the database migration","marks":[[12,18]],…}]}`, newer first; choosing the
second opens its thread with the focus settled (`message-opened`). Its screenshots showed a second clear button, the
browser's own on a `type="search"` input, which became a text input.

**apack PR #148** (MiniSearch message search, rebuilt in memory at each boot) is superseded: reported to the user, whose
to close.
