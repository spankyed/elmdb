# Phase 9 of goal-elmdb-next: substring and regex search

2026-10-27, elmdb `7dd8a7f`. The design is `docs/architecture/search.md`, Substrings and patterns as built; the
declaration `schema.md`, Indexes; the key `format.md`, Keys.

## What was built

- **The declaration**: `{ "kind": "trigram", "fields": [...] }` on string, list or `any` fields, with no options.
  apack's manifest entry declares it as `substrings` (apack `a983092538`, PR #285), and default-setup's Message declares it on
  `text`.
- **The index**: each run of three characters a field's text holds, case folded character by character, with the
  places of the entities holding it as a roaring bitmap, one key per trigram and block of 2^16 places. A field write
  refiles what its text gained and lost in the same unit; destroying an entity takes it out; an index declared over
  rows is built in the background, as a value index is; `check()` derives it and `rebuild()`, whole or of the one
  index, makes it; dropping it deletes its keys.
- **The steps**: `containing` (a text, exactly or with `caseInsensitive`) and `matching` (Rust's `regex` syntax, G11).
  What every match holds is read off the text, or off the pattern's syntax (literals; each part of a sequence; one
  branch of an alternation; a repetition's body when it repeats at least once), the candidates intersected or joined,
  and each candidate checked against its text. What can't narrow — a text under three characters, a class, an
  alternation with such a branch, an index not built yet — reads every entity of the type and checks each.
- **Refusals**: a pattern that doesn't parse, or uses look-around or backreferences, `PATTERN_INVALID` with the
  parser's reason and the pattern; a type with no trigram index, `UNKNOWN_INDEX`, as a search's.

## Equal to a scan

- **The model harness** (`engine/tests/model.rs`): Note's fields `a` and `b` carry a trigram index, and after every
  step of every workload nine searches — four `containing` (one under three characters, one ignoring case), five
  `matching` — each in three forms (a type start live, a type start with the trash, a set of both types, whose Tasks
  have no index) equal the model's scan of its own rows. `check()` is clean after each workload.
- **A fixed corpus** (`engine/tests/trigram.rs`): a chat's errors, ids, a hash, a path and code, in several cases and
  in a list: ten substrings exactly and ignoring case, and ten patterns, equal a scan of the corpus; the same after
  the index is built over rows written before it was declared. Writes (a set, a delete, a destroy, an append, a trash)
  are followed; a block missing a place and a block nothing derives are what `check()` reports, `rebuild()` of the
  index and of everything repairs them, and a dropped index leaves no key.
- **G8's data** (below): eight searches, each equal to a scan of every Message's text.
- **Mutations**, each on a copy-on-write clone, each failing the specs: an alternation skipping a branch that can't
  narrow; a write keeping the trigrams its text lost; trigrams not folded; an optional repetition read as required
  (which survived the first corpus, and is why `(Type)?Error: ` is in it); destroy leaving its trigrams; a list's
  elements read as no text; an emptied block kept; a build dropping its last block.

## On G8's data

G8's dump loaded by the `elmdb` CLI built at each layout, in a copy-on-write clone; Message (19,681) declared with
`text` under the index through the binding and built; 200 messages taken from the data created and each grown twice
before the index and 200 after it, every write its own commit; then each search and a scan of every Message's text
(read as rows, checked in JavaScript), medians of seven; the database compacted before and after (2026-10-27, Apple
M1 Pro, load 10 to 16):

| Layout | Compacted file | The index | Build | A message created | A message grown |
|---|---:|---:|---:|---:|---:|
| no index | 122.1 MB | | | 0.071 ms | 0.046 ms |
| a key per trigram and entity | 366.2 MB | +244.0 MB | 5.2 s | | |
| blocks of 2^10 places | 158.9 MB | +36.8 MB | 5.7 s | 0.455 ms | 0.196 ms |
| blocks of 2^16 places | 138.6 MB | +16.5 MB | 8.5 s | 0.381 ms | 0.163 ms |
| one bitmap per trigram (2^40) | 137.9 MB | +15.8 MB | 8.8 s | 0.619 ms | 0.224 ms |

A key per trigram and entity, the first layout built, tripled the file: every entry repeats its type, index and
trigram. Blocks of 2^16 hold the index in an eighth of the file. 2^10 takes twice that in more
keys, and one bitmap per trigram costs a create more, a common trigram's whole bitmap rewritten by every message
holding it, which a block bounds. A created message of 258 characters on average writes about 0.3 ms more for its
trigrams; a message growing as an answer streams in writes 0.12 ms more, its new trigrams few.

The searches, at 2^16 places (the other layouts read the same candidates, within a few percent of each other's times):

| Search | Matches | Entities read | Index | Scan |
|---|---:|---:|---:|---:|
| `containing("error:")` | 30 | 135 | 1.44 ms | 69.0 ms |
| `containing("the")` | 9,893 | 10,064 | 39.8 ms | 59.5 ms |
| `matching("npm run (chain\|spec)")` | 0 | 55 | 0.71 ms | 61.1 ms |
| `matching("[0-9a-f]{10}")` | 127 | 20,081 (every one) | 53.8 ms | 90.7 ms |
| `containing("ETIMEDOUT")`, `("THR-")`, `("packages/apack-ears")`, `matching("THR-\d+")` | 0 | 0 | 0.01 to 0.05 ms | 59 to 61 ms |

A rare substring is found in a millisecond or less, against a scan of 60 ms; a pattern that narrows by nothing reads
every message, as the scan does, and is faster only by not decoding rows into JavaScript. Every answer equals the
scan's, and `check()` reports no difference after the build and the writes.

## G4

apack's `npm run bench -w @apackdev/ears`, the engine over elmdb `78ed769` (before, built in a copy-on-write clone) and
`7dd8a7f` (after), alternated twice through the `vendor/elmdb` link at a load of 10 to 12; the lower of each case's two
medians, in ms. The bench declares no trigram index, so it reads what the change costs a store without one: a check of
the type's indexes in each field write, and the plan's new steps.

| Case | Before | After | |
|---|---:|---:|---:|
| bulk load 50k entities, 200k attributes, 100k relations | 1336.23 | 1348.03 | +0.9% |
| 1,000 where lookups by a unique value | 7.89 | 7.74 | −1.9% |
| 1,000 getRow | 5.53 | 5.56 | +0.5% |
| pickAll over 40k | 92.20 | 92.83 | +0.7% |
| link 10k tasks to one project, then destroy it | 890.13 | 881.07 | −1.0% |
| 1,000 creates | 133.23 | 131.59 | −1.2% |
| transact: 1,000 creates as one unit | 26.21 | 26.44 | +0.9% |
| 1,000 creates, then 100 watches on a page of the type run again | 134.65 | 134.11 | −0.4% |
| 1,000 page reads | 86.00 | 85.77 | −0.3% |
| a message write at 50,000 | 0.63 | 0.63 | +0.3% |
| a fuzzy search at 1,000 | 0.87 | 0.91 | +4.0% |
| a common word searched exactly at 50,000 | 21.07 | 20.83 | −1.1% |

Every other case is within ±3.9%; none is past the 10% tolerance. What a trigram index costs a write is the reading
on G8's data above.
