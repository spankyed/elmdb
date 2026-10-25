# Phase 14: change sets, tracked reads and `engine.watch`

2026-10-25, MacBook Pro (Apple silicon), macOS 25.3, Node 23.11.1, Rust 1.99.1. elmdb `fd0c0d7`; apack's worktree
`~/Develop/Projects/AgentBuddy-worktrees/ears-on-elmdb`, branch `session/ears-on-elmdb`, pinning `fd0c0d7`.

## In elmdb

- **The sequence**: every commit raises `m/sequence` by one inside its own transaction (`Store::commit_txn`), so a
  commit and its number are one; a reset's and a replace's commits keep it rising, and a reopen reads it back. A
  batch's result, its change set and a unit's commit carry it; `db.sequence` is the last.
- **Tracked reads** (`engine/tracking.rs`): `Recording` wraps a reader and records each key `get` was asked for, present
  or not, and each scan's range, to just past the last key a stopped scan was shown, the whole range for one that ran
  out or failed. A read through hannoy, which elmdb can't see into, notes its index's items (`Read::note`), which
  every vector added or removed writes. `TrackIndex` files read sets per owner: a key map, and an interval treap
  ordered by `(low, id)` with each subtree's highest end, so a written key costs a lookup and a stabbing query of
  O(log N + matches). `{ reads, track: token }` adds to what the token holds; `untrack` drops it (D32).
- **Commits name tokens** (`engine/watch.rs`): a unit logs the keys it writes (`UnitState::written`) while something
  is tracked as it begins, and its change set's `touched` is the tokens those keys fall in. A commit whose writes
  aren't logged key by key — reset, replace, a schema change, a load, an admin unit — touches everything.
- **`Database::watch`**: reads run tracked under a watch's own owner, run again after each commit that names it,
  the listener handed each answer that differs from the last, after the commit and outside its transaction.

**Checks.** `cargo test --workspace`: 170 pass, among them the index against a brute-force scan (500 random
workloads of adds, removals and written keys, over keys that meet, nest and share ends) and the tracked specs: a
commit names the tokens it wrote into and no others — another entity, an unread field, an untracked token — and a key
found absent counts; a stopped scan is tracked as far as it went; a unit names its tokens as it commits, and a refused
batch and an aborted unit commit nothing; the sequence through a reset and a reopen; a watch delivering each changed
answer and not an equal one, hearing a reset, and leaving nothing filed once ended. The binding's 19 tests, one
tracking through it from JavaScript, pass. Clippy and fmt are clean.

**Mutation checks**, each restored: not logging a `put` fails three tracked specs; ending a stopped scan's range at
its last key instead of past it fails the stopped-scan spec; a stabbing query that skips a range at its low end fails
both index specs.

## In apack

- **`engine.watch`** (`@apackdev/ears`, `watch.ts`), keeping live-queries' contract exactly — `watch(read, observer)`
  and `lastCommit()` on the engine, `watchesSettled()` and `watchStats()` on `admin` — over tracked reads: a run drops
  what the watch's token held and makes its reads under it (`Connection.track`), each write's and commit's change set
  hands its `touched` tokens to the commit listeners, and the re-runs are coalesced into one flush after the writing
  code returns, compared with `sameValue` and delivered with the sequence. `admin.clear`, `admin.replaceWith` (new),
  `admin.setSchema`, `admin.declare` and loads are heard as touching everything. The host's app reset and backup
  import empty and replace the app's database through the engine, so its watches hear them.
- **The acceptance is live-queries' own**, ported from `517900b52`: `watch.spec.ts`'s 12 cases pass unchanged; the
  model harness's watch property — a watch on every derived read, every row and the whole state equal to the read run
  afresh after every op of a random workload — passes (40 workloads); the bench's watch cases are recorded below.
- **Mutation checks**, each restored: not handing a write's change set to the listeners fails 8 cases; ending a stopped
  scan's range at its last key (in elmdb, through a rebuilt binding) fails the limited-read case and the harness
  property; notifying for a unit that threw fails the refused-unit case.

**The bench** (`npm run bench -w @apackdev/ears -- -t watches`, elmdb `fd0c0d7`, no other load): 1,000 creates with
watches open on other rows, mean of 20 iterations — 0 watches 175 ms (±6.6%), 10 watches 210 ms (±10.8%), 100 watches
159 ms (±7.2%), 1,000 watches 164 ms (±6.0%): flat, within the runs' spread. 1,000 creates then 100 watches on a page
of the type run again: 141 ms (±5.0%).

**The gate.** apack's `npm run chain -- --no-native` passed on the Phase 14 tree (2026-10-25, 1,178.2s), committed as
`cf810c91d`. The run before it failed in `build:default-setup` because a file not part of this work
(`crates/elmdb-node/protocol/Change copy.js`) was edited inside the linked package while the pack compiler had it
captured (`Declared npm package membership changed`); the build passed alone, and so did the next chain.
