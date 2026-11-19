# Roadmap

What is built, the road to 1.0, what is designed and waiting, and what comes later.

## To 1.0

Ten changes, each done when its gate holds, set on 2026-11-09 from a review of `ed20979`. They come before the designs
waiting on a trigger below, which add to them and reorder nothing.

| # | Change | Done when | State |
|---|---|---|---|
| 1 | The defects the review reproduced | each is fixed with a test that failed first; the model harness covers change sets, `changedSince`, `reset`, `renameRelation` and `renameField`; `renameType` and `replaceWith` have deterministic engine tests, and purge has them for types and fields | fixed, `79f646f`; the model harness covers change sets, `changedSince`, `reset`, `renameRelation` and `renameField` (#117, #118, #119, #121, #122), and since then a field's and a relation kind's purge, a replacement by a backup and `renameType` (#168); a type purge and a role purge have deterministic engine tests (`architecture/testing.md`, The model harness). The gate was amended to the first coverage, by the author's decision. Built |
| 2 | Panics caught at the binding | a failpoint's panic in each export is a JavaScript error with a code of its own, and the process lives | built: `PANICKED` (`architecture/engine.md`, Errors; `architecture/testing.md`, The binding) |
| 3 | A mode that survives power loss | an open option commits with LMDB's own sync; a crash-replay harness passes it and fails the grouped mode once; the README states the trade | built: `syncEachCommit` (`architecture/engine.md`, Durability; `architecture/testing.md`, Power loss); the README states the trade. Since the gate was set the grouped mode keeps its last synced commit and restores it after a power loss (`roadmap/synced-commit.md`: phases 1 to 5 built, but for phase 5's steps by hand on each platform), so the replay holds both modes, and fails the grouped one only with its pin turned off |
| 4 | `check`, `rebuild` and `backup` off the calling thread | the event loop is never held 10 ms during a rebuild of 200,000 notes that no write waits on | built (#113); the quiet reading holds the loop under 3 ms for a 200,000-note rebuild on the pool (`evidence/million-notes.md`, off the calling thread). `write` left the item on 2026-11-15, by the owner's decision that reads and writes block (`architecture/design.md`, principle 9): its promise form answered what EARS hands back at the call too late, and a write made during a rebuild now waits for it on the calling thread |
| 5 | Ranges, ordered reads and a cursor that seeks | `orderBy` and `limit 10` on an indexed field visit about ten entities, and a page costs the same wherever its cursor is | built for booleans, numbers, the engine's times and strings: ten of a million notes read for their first ten, fifty for a page of fifty near either order's end, and a string's order read from the keys its index keeps in the collation's order (`archive/roadmap/string-order.md`, phases 1 to 4; `evidence/million-notes.md`) |
| 6 | Vector builds off the commit path | one embedding a commit costs within twice a batch's; a commit of one takes under 2 ms at 100,000 vectors | open |
| 7 | Typed answers and a thin client | answers are generated as the rest of the protocol is, and a test decodes each into its type | built: a write's answer is generated (`Written`, `protocol/Written.ts`), and so is every answer a read or a plan gives (`Answers` and `Answer`, `engine/answer.rs`, generated as `protocol/Answer.ts`), which the binding's declarations name. `engine/tests/answers.rs` makes a read of each of the 24 kinds and `answer::conforms` holds its bytes to its type, every key and none more, as it holds every read the crate's own tests make; `schema-types.test.mjs` decodes a write and a read of 15 kinds into their generated types (`architecture/testing.md`, The binding). The thin client, `elmdb/client.js`, takes and answers those types over the caller's msgpack codec, decoding exactly an answer that may hold `__proto__` (`architecture/design.md`, The boundary with apack; `client.test.mjs`) |
| 8 | Format 1 frozen | databases and dumps of every tag open and pass `check()` in CI; the move to LMDB 1.0 is decided first | designed (`roadmap/format-freeze.md`): format 1 stays on LMDB 0.9, edited in place with no bump; index-derived data stays out of the lock, each index recording its layout and one of another thrown away and built again. Not built |
| 9 | Release and supply chain | prebuilt binaries for the seven targets; `cargo-deny` and an audit in CI; the `$schema` URL resolves | the prebuilt binaries are built: `release.yml` packs all seven into one npm package, installs it on each target and, on a tag, drafts a GitHub release with an SBOM of each binary (`architecture/testing.md`, Releases); its dry run passed on all seven targets on 2026-11-18, on the workflow as merged (run 44380480334, at `9c6e765`). No release is tagged and nothing is published to npm; `cargo-deny`, the audit and the `$schema` URL are planned, not built (`roadmap/deferred/supply-chain.md`): `elmdb.dev`, the schema language's domain, isn't registered, which is to be settled before 0.3 |
| 10 | A benchmark against SQLite, committed | bytes per note, search and rebuild at a million notes are read for each release | the benchmark is built (`architecture/testing.md`, Against SQLite) and read at a million notes on six days from 2026-11-10 to 2026-11-15 (`evidence/million-notes.md`); no release is tagged, so none is a release's reading |

| Milestone | Holds | Reached when |
|---|---|---|
| 0.1, safe to embed | 1 to 4 | no input known aborts or hangs the host, and power loss has a tested answer |
| 0.2, the queries a stranger expects | 5, compound indexes read, the smaller filter first, least, greatest and sum | the README's uses need no filtering in JavaScript |
| 0.3, contracts frozen | 7, 8 | the format, the protocol and the error codes each have a compatibility test |
| 0.4, installable | 9, a quickstart, a cookbook of plans, a decision on Chinese, Japanese and Korean text | `npm install elmdb` works on the seven targets and a second program is built on it |
| 0.5, a known envelope | 6, 10 | readings at a million notes are published, each with its target |
| 1.0 | | two releases break neither the format nor the protocol, and an upgrade from 0.3 is tested |

Milestone 0.1's gate holds as far as it is known: changes 1 to 4 are built, so the inputs the review found to abort
or hang the host are fixed and a panic is an error at the binding, and power loss has a tested answer, in the replay
and over LazyFS.
What is left is outside the gate: no machine's power has been cut under a test (`roadmap/synced-commit.md`, phase 5's
steps by hand).

Of milestone 0.2, compound indexes are read for an order or a range under equalities, and with none the order is
walked while that is cheaper than sorting the filter's matches (`evidence/million-notes.md`, A filter before an
order). The smaller filter first is built as the places the filters' indexes hold in common, a read bounded by the
shortest of them whichever is written first, equalities on every field of a compound index among
them as one list (`architecture/engine.md`, Reads: plans). Least, greatest and sum are the plan's terminals `least`, `greatest` and `sum` (`architecture/engine.md`, Reads: plans).

Milestone 0.2 is reached, its uses read against the plan's steps on 2026-11-16 at `387e8c4`: the README's search of a
misspelled word and its most central note are `search` and `ranked` with `limit`, and its tracked reads are `track`;
the uses it lists are `search` for text, `near`, `like` and `hybrid` for vectors, `containing` and `matching` for
substrings and patterns, and `pathTo`, the `communities` terminal and `rank` or `ranked` for graphs. None needs a filter in
JavaScript. What a stranger's first queries would meet, and the README doesn't use: no filter keeps a field holding
one of several values, a value other than one, or either of two filters, so a caller reads twice and joins the
answers, or filters in JavaScript.

The benchmark comes first, out of its milestone, since it is what the gates below are read from. The indexes' layout
at a million notes is designed in `roadmap/text-index-at-scale.md`, and its first step, a term's places in blocks and
ranking read from them, is built. Its journal changes only keys under an index, which the freeze leaves to each index's
recorded layout (`roadmap/format-freeze.md`), so it can land after the freeze once indexes record theirs; its last
step moves a type's entities in creation order, which the freeze locks.
Change 6 waits for that design's journal, since a vector index's journal outliving its commit is the same design.

## Built

| What | Record |
|---|---|
| The protocol's cost, measured against lmdb-js | `evidence/phase-1.md` |
| Parity with apack's engine, and apack off lmdb-js | `evidence/phase-9.md`, `phase-10.md` |
| A second database for apack's run history | `archive/goals/goal-elmdb.md`, Phase 11 |
| Ranked text search, fuzzy labels | `evidence/phase-12.md` |
| Vector and hybrid search | `evidence/phase-13.md` |
| Change sets, tracked reads and watch | `evidence/phase-14.md` |
| Call counts, bounded watches, EXPLAIN, field types, compressed values, trigram search | `evidence/next-phase-3.md` to `next-phase-9.md` |
| Graph algorithms, and their weights | [graph-algorithms](archive/roadmap/graph-algorithms.md), [rank-weights](archive/roadmap/rank-weights.md) |
| Rank indexes | [rank-index](archive/roadmap/rank-index.md), `evidence/rank-index.md` |
| Filters read as the places their indexes hold in common, a value index's places in blocks, a search within a filter's set | [plan-algebra](archive/roadmap/plan-algebra.md), `evidence/million-notes.md` |
| Graph walks: `descendants`, `ancestors` and `pathTo` with a depth, `degrees` and `edges` | `roadmap/deferred/graph-traversal.md`, What a plan answers now |
| The catalog read | `architecture/engine.md`, Reads: plans |
| Commits in other processes: `committedElsewhere`, a wake for each, and a replacement's epoch | `architecture/engine.md`, Schema |
| What changed since a position, and a reader's checkpoint | `architecture/engine.md`, Changed since and Checkpoints |
| Orders, ranges and pages read from a value index, a string's among them | `archive/roadmap/string-order.md`, `evidence/million-notes.md` |
| `least`, `greatest` and `sum` | `architecture/engine.md`, Reads: plans |
| Panics caught at the binding; `check`, `rebuild`, `backup` and `read` on the pool, a `readAsync` answering as at its call | `architecture/engine.md`, Errors and Reads: plans |
| The last synced commit kept through a power loss, and one mode of writing a database at a time | `roadmap/synced-commit.md`, `architecture/engine.md`, Durability |
| The text index at a million notes: a term's places in blocks, lengths by place, ranking read from the two | `roadmap/text-index-at-scale.md`, `evidence/million-notes.md` |

Other built directions are in `archive/roadmap/`.

## Designed, each waiting for its trigger

Each is a design in `roadmap/` with its measurements (`evidence/next-phase-10.md` for the change log, the views, the
boundary and the dictionaries; `evidence/million-notes.md` for the text index), built when the reading it names calls
for it:

- **Incremental backup as the fields changed since a sequence** (`roadmap/change-log.md`): when apack's backup screen
  takes incremental backups.
- **Maintained views** (`roadmap/incremental-views.md`): when a view's re-run is a measurable share of a user action.
- **Lazy rows and optimistic units** (`roadmap/javascript-boundary.md`): when a view decodes fields it doesn't read,
  or a flow holds a unit across an `await`.
- **Per-type dictionaries** (`roadmap/value-dictionaries.md`): when a type's small text values are a tenth of the file.
- **Block-max scores** (`roadmap/text-index-at-scale.md`): when a common word's search is measured slow. Ranking reads
  its counts and lengths from the index, and the best ten for a word every note holds took 20.6 ms at a million notes
  (2026-11-10, `evidence/million-notes.md`, With a term's places in blocks).
- **A commit that notes the place it changed, the builder filing it** (`roadmap/text-index-at-scale.md`): the journal
  change 6 waits for.
- **What graph query languages answer beyond a plan** (`roadmap/deferred/graph-traversal.md`): each when a caller needs it.

## Later, each its own decision

- LMDB 1.0 through `heed3`, crossed by a dump and a load, once hannoy runs on heed3 and heed3's LMDB is a 1.0 release:
  encryption and incremental backup (`roadmap/format-freeze.md`).
- Scratch files with a configured placement and quota, reclaimed after a hard exit (`elmdb-check-*` directories a SIGKILL
  leaves; `architecture/engine.md`, Recovery resources).
- `NOT_DURABLE` naming the tokens its commit touched, so a caller invalidates those rather than every watch.
- Populated replacement measured across broader profiles, and each recovery operation on its own.
