# EARS in elmdb's repository

Status: planned, 2026-11-14; nothing is built. This is the second draft: an independent read of the first, against
both repositories, changed it (What the review changed, at the end).

EARS, the entity-attribute-relation system, is the fluent TypeScript API over an elmdb database: what a program
holds (`createEarsEngine({ database })`), its query and write builders, which compile to plans and batches, its
watches, and its typed facades. It is apack's `@apackdev/ears` today: `packages/apack-ears` in `spankyed/apack`,
3,873 lines under `src/` and 4,521 under `tests/` in 20 spec files (`wc -l` over `git ls-files` at `3d5f18867a`).
This plan moves all of it here, as a package of its own beside the binding, and says what apack keeps.

In this document "the engine" is the Rust crate, as everywhere in this repository. apack calls EARS an engine too
(`createEarsEngine`); that name is one of the decisions left for after the move.

`../architecture/design.md` (The boundary with apack) decided the opposite on 2026-10-25, and named what would
reverse it: "If the builders settle and another program wants them, they move into elmdb then." No other program
wants them. Whether they have settled is not known: `src/` changed in 68 commits between 2026-10-25 and 2026-11-06,
while EARS was being moved onto elmdb, and in none in the eight days since. The move is decided for the reasons
below, with its cost counted, and that section of `design.md` is rewritten when phase 3 lands.

## Why move it

- **The builders are the only way anyone uses elmdb from JavaScript.** The binding takes msgpack plans and batches,
  which nobody writes by hand. A database whose usable API lives in its one user's repository has none of its own:
  the README can't show a query, and a second program can't start.
- **EARS' specs are the widest test of the engine from outside, and this repository's CI doesn't run them.** Sixteen
  black-box spec files, a reference model driven by random workloads with a watch on every read it checks, and the
  watch contract. They run in apack, in a macOS job, after an engine change has merged here and apack's pin has
  moved to it. The pin moved 88 times between 2026-10-25 and 2026-11-06, and has not moved since: apack runs commit
  `4b67670`, 324 commits behind `bf4b21b`, and one of them (`6616fd3`, 2026-11-12) made the binding's `write` and
  `backup` answer a promise. EARS calls `write` and reads its answer at once, so EARS as it stands does not run on
  this repository's `main`, and nothing here says so.
- **A protocol change and the builder that uses it become one pull request**, tested together before either merges.

One reason the first draft gave is not a reason to move: EARS' hand-written copies of the protocol's types (`Op` as
`{ op: string } & Record<string, unknown>`, the schema `toElmdbSchema` returns as `object`, a table of 21 error
codes, a port reached through `as unknown as EarsDatabase`) can be typed from the binding's shipped `protocol/`
where EARS is now, since the one module that loads elmdb already imports its `Schema`. That work is phase 0.

## What it costs

Counted over the 68 commits, merges aside, that touched `packages/apack-ears/src` between 2026-10-25 and 2026-11-06
(`git log --no-merges --since=2026-10-25 -- packages/apack-ears/src` in apack at `3d5f18867a`, each commit's files
read; "other apack code" is any file outside the package that is not a document, a changeset or the lockfile):

| The commit also | Commits | Today | After the move |
|---|---|---|---|
| moved the elmdb pin and touched other apack code | 26 | two repositories | two repositories |
| moved the elmdb pin only | 5 | two repositories | one |
| touched other apack code only | 25 | one | two repositories |
| neither | 12 | one | one |

31 of 68 changes cross repositories today; 51 would have. **On the only data there is, the move makes a change that
crosses repositories more common, not less.** That data is twelve days of building EARS onto elmdb, when every
change touched everything; what a settled EARS looks like has not been observed. It is also `design.md`'s own
reason for the boundary it drew: the builders change with apack's vocabulary.

What the move buys for that is which half of a change is whole. Today an engine change merges here untested by the
API that uses it. Afterwards the engine and its API merge together, tested on three platforms, and apack's half is
the SDK and host code that uses a finished thing.

The other costs:

- **The typed contract is split.** EARS' `entities.ts`, `runtime.ts` and `typed.ts` are change-controlled with the
  SDK's types and apack's codegen (apack's `TYPED-EARS.md`). The checks that caught what compiling missed, a
  language service asked for completions over two built packs (`facade-typing.integration.spec.ts`), need apack's
  codegen and stay there. A change to those three files here meets them only when apack's pin moves.
- **Code the mutation gate doesn't read.** `mutants-pass` mutates Rust. Nothing proves EARS' specs catch the
  mutants nobody thought of.
- **A second toolchain here**: `tsc`, vitest, fast-check, api-extractor, and a second TypeScript at EARS' floor
  (5.7) for the check that a consumer compiles. apack's build of the package is more than `tsc`
  (`scripts/build-package.ts`: a staged emit, the declarations' import extensions rewritten, every export target
  checked); those steps import apack's host and are written again here.
- **Every EARS change waits on this repository's three required checks**, a TypeScript-only one included.
- **Trying a TypeScript-only EARS change against apack needs an elmdb checkout with a built binding and a built
  `dist`.** Today it needs neither rebuilt.

## The alternatives, and why not

- **Type EARS from `protocol/` where it is, and stop.** Gets the compiler's checks, and leaves the first three
  reasons unanswered. It is phase 0, worth having either way.
- **A job here that runs apack's EARS suite against the pull request's binding.** Gets the second reason without
  moving anything. But a pull request that changes the protocol fails it until apack's EARS follows, which can't
  merge until the pull request does: the job could only ever be advisory, and this repository's CI would read
  another repository's head.
- **Move the port, the codec and the compilers; leave the typed contract in apack.** Keeps the contract beside its
  codegen. But `runtime.ts` is the builders' interface: it would split each builder from its type across two
  repositories, which is the worst place for the line.
- **A subpath of the binding's package (`elmdb/ears`).** A mismatched pair becomes impossible by construction. The
  same is had by one pin and one version, and the binding stays napi-rs's generated CommonJS loader with no
  dependency, in a crate's directory.

## What moves, what stays, what goes

**Moves, whole**: `packages/apack-ears`: `src/`, `tests/`, `bench/` (whose readings are already here, in
`../evidence/phase-9.md` and `../evidence/next-phase-3.md`), `etc/` (the API reports) and its `CLAUDE.md`, as
`../architecture/ears.md`. It imports no apack package. What ties it to apack is cut in phase 0:

- `query.ts`: the unindexed-walk warning says "Declare it in the … entry of apack.json". The message names the type
  and the field; apack's `warn` adds where a pack declares an index.
- `calls.ts`: `ENGINE_FRAME` finds EARS' own stack frames by the path `apack-ears` or `@apackdev/ears`, and
  `shortened` cuts paths at `/packages/` and `/packs/`. Both become what its creator passes, with defaults that
  name no program.
- `vitest.config.ts`: two setup files from `packages/apack-testing`, one of which loads apack's SDK, and vitest's
  own deadlines switched off.
- Comments and exported prose naming packs, manifests, codegen and `#generated/ears`.

**Written here, not moved** (they prove EARS' types and published form through apack's packs today; each is written
again over a schema of two made-up types, and apack keeps its own, which test its codegen): the type assertions of
default-setup's `typed-query-builder.spec.ts` and `branded-entity-id.spec.ts`; a completion check through the
language service, as `facade-typing` makes over built packs; the EARS halves of `published-exports`,
`published-sdk-types` and `published-sdk-any` (the packed package compiles for a consumer with no Node types, on
TypeScript 5.7 and the current one); and the rule apack's import guard holds inside EARS' source (`ELMDB_RULES`'
second entry): nothing the root entry reaches imports `elmdb` or a Node module.

**Stays in apack**: the bound database and the operations over it (`@apackdev/sdk/ears`), the SDK's `EARS`
namespace with its entity names, codegen, the schema assembled from pack manifests, the Database console, backup,
and their specs. The pack rule that a pack loads no database (`elmdb-imports`) stays, with the new names.

**Goes from apack** once nothing there builds EARS: its row in the import guard's package table, and `ELMDB_RULES`'
second entry with its fixtures; `SOURCE_PACKAGES`, `PUBLISHED_TREES`, `BUILD_UNITS`, and `INLINED_SOURCE_DIRS`,
which derives a directory from an `@apackdev/` name; its place in the changesets fixed group and the TypeScript
floor's list; `typecheck:ears`, its API reports' check, its vitest project, its unit-suite size, its timeout
exemption, its bench's typecheck leg; the API's `--watch ../apack-ears/src`; the EARS halves of `publish-checks`;
`apack init`'s scaffolded dependency, `shared-ears.ts`'s check for it, and the pack build's refusal of a pack that
can't resolve it (`phase-cache.ts`, `tool-identity.ts`, `checkout-packages.ts`).

## Decisions

**The package is `@elmdb/ears`, in `packages/ears`.** The name people say is EARS. The npm name `ears` is someone
else's (`ears@0.3.1`, an HTTP messaging framework; `npm view`, 2026-11-14), so a local package of that name could
never be published, and until then one mistyped install fetches a stranger's code into an app that runs packs
in-process. `@elmdb/ears`, `elmdb-ears` and `elmdb` answered 404 (same reading). The scope is not held: registering
it is the owner's, before anything is published, and `elmdb-ears` is the name if it can't be had. It is not under
`crates/`, which holds crates; `design.md`'s Packages table, whose rule is one product name with the host language
as the suffix, gains a row and a sentence for a package that is no binding.

```
crates/elmdb, crates/elmdb-node (npm `elmdb`), crates/elmdb-cli, xtask
packages/ears/            npm `@elmdb/ears`
  src/  tests/  bench/  etc/  package.json  tsconfig.json  tsconfig.package.json  vitest.config.ts
package.json              private; workspaces: crates/elmdb-node, packages/ears; all tooling
```

From the root, as now: `npm ci`, `npm run build` (the binding, then EARS' `dist`), `npm run typecheck`, `npm test`
(the binding's `node --test`, then EARS' vitest), which `cargo xtask pre-push` and CI's binding job already run. CI
changes with it: the binding job's Node moves from 23 to one vitest supports (4.1.12 declares `^20 || ^22 || >=24`);
the Windows job, which builds the binding with cargo and never runs `npm run build`, builds `dist`; and the change
filter, which reads any `.md` as a document, stops reading `packages/ears/etc/*.api.md` as one. An npm workspace at
the root leaves apack's link to `crates/elmdb-node` as it is (the review checked the link, the lockfile entry and
the packaged app's rules).

**`elmdb` is EARS' peer, not its dependency.** One process holds one binding (`design.md`, principle 5), and whoever
opens the database provides it; the root entry never loads it. A dependency at an exact version would also be a
registry lookup of an unregistered name wherever it wasn't already satisfied. `msgpackr` is its one dependency. Its
manifest holds no tooling. apack's CI and its Linux container run `npm ci` at this repository's root to build the
binding, so they do install the root's tooling, EARS' included, as they install the binding's today.

**Two entries, as now, one renamed.** `@elmdb/ears` is the builders and their types: it loads no native code and
names no Node API, so a browser bundle can hold its constants and helpers, as apack's frontends do, and its
declarations name neither `elmdb` nor `Buffer`. `@elmdb/ears/database` is today's `/lmdb` (which opens an elmdb
database, not an LMDB one): `openDatabase`, `createTemporaryDatabases` and `DATABASE_FORMAT`.

**The port stays structural.** `EarsDatabase` remains EARS' own statement of what it needs, in `Uint8Array`s, which
a spec can wrap with an object of its own. What goes is the cast in the one module that imports `elmdb`:
`openDatabase` returns the binding's `Database` as an `EarsDatabase` with no `as`, so a changed signature fails to
compile there. Making the port the binding's own type, as the first draft had it, would put `Buffer` and `elmdb`
into the root entry's declarations.

**Every export keeps its name and meaning through the move.** Nothing is renamed, so the moved specs are the proof
that nothing changed, and apack's change is the package's name, which 173 files outside the package name today
(`git grep -l '@apackdev/ears'` at `3d5f18867a`, documents and the lockfile aside).

**A pack names only apack's SDK.** Today a pack depends on `@apackdev/ears` by name: `apack init` scaffolds it, the
generated `#generated/ears` and `#generated/services` import it, and default-setup imports it in four source files.
The SDK exports what those use. Generated code takes `EntityId`, `AttrKind` and the rest from the `EARS` namespace
the SDK already exports, not a second one; `@apackdev/sdk/ears` gains `defineEars`, `EarsSchema`, `UntypedSchema`,
`ShapeOf`, `ShortCode`, `BaseEntity`, `QueryBuilder`, `WriteBuilder`, `RelationRow`, `RelationMatch`,
`RelationStats`, `QueryExplanation` and `RESERVED_ATTRIBUTES`, none of which it exports today. A pack then has one
dependency whose version the app decides, and EARS' package is the SDK's concern. The review traced this through
pack backends (the SDK is bridged), pack frontends (their imports are types, and EARS is free of side effects) and
generated facades, and found it holds. Whether EARS can then leave apack's shared-instance list is not shown
(Unproven).

**apack consumes it as it consumes the binding**: `"@elmdb/ears": "file:../../vendor/elmdb/packages/ears"`, through
the same link and the one pin, which then names a tested pair. It consumes `dist`: the `@apackdev/source` condition
is apack's and goes. Two things follow, and both are handled rather than hoped for. `dist` is a build output in a
checkout whose branch changes, so the build writes into it a digest of the `src/` it was built from, and apack's
link script and test runner refuse a `dist` whose digest isn't its source's, as they refuse a missing `.node` now.
And Node resolves a linked package's imports from where its files really are, so EARS' `msgpackr` is the one this
repository's `npm ci` installed, not apack's lockfile's.

**History comes with it.** The package's 225 commits are imported as a subtree and merged, so `git blame` on EARS
survives. Both repositories are public (`gh repo view`, 2026-11-14), so nothing is published that wasn't. The
commits keep their messages; an `#N` in one will link to this repository's issue of that number, which is wrong and
left. The package's licence goes from MIT to this repository's `MIT OR Apache-2.0`, which is its one author's to do.

## What it unlocks

From phase 2, held by CI: every pull request here runs EARS' specs and model harness on Linux, Apple silicon and
Windows before it merges (they have only ever run in CI on macOS). No plan step exists without a builder, and no
builder waits on a pin. The benchmarks sit beside the records that cite them. The README opens a database and runs
a query.

From phase 0, held by the compiler, and had without the move: a builder's output is a generated `Plan` or `Op`, so
a protocol field renamed in Rust fails every builder that writes it; `toElmdbSchema` returns the generated
`Schema`; the error table is checked against the generated `ErrorCode`, so a new refusal is mapped or listed as
passed through; `openDatabase` has no cast.

Not had, and not promised: a read's answers have no generated type. The engine writes each of its 19 kinds by hand
(`engine/reads.rs`), and a write's envelope (`results`, `changes`, `sequence`) is described only in a comment. Types
for them are new work in the engine, listed below.

Opened by having both halves in one repository, each to be decided on a reading, none part of the move:

- **Logic in two languages** (`design.md`, principle 1). EARS reads whether an id exists before `write(id)` writes
  (a crossing per write, which `javascript-boundary.md` measured as the largest loop in a chat turn), checks
  relation kind and role names the batch checks again (`names.ts`), checks a short code's form, stamps `updatedAt`,
  and schedules watches.
- **Errors.** EARS renames three of the engine's codes (`UNKNOWN_TYPE`, `RESERVED_FIELD`, `RESERVED_TYPE`) and
  defines three of its own in TypeScript (`NO_VALUE`, `NO_SEARCH`, `NOT_EXPLAINABLE`), against this repository's
  rule that errors are `ErrorCode`s.
- **Names**: `createEarsEngine` and `EarsEngine`, where "engine" means the Rust crate.
- **Answers as Rust types**, generated like the rest.
- **`DATABASE_FORMAT` from the Rust constant**; the binding exports none today.
- **Mutation testing for TypeScript.**

## Phases

0. **In apack, before anything moves** (`spankyed/apack`; each step checked by apack's suites, and each worth having
   if the rest never follows).
   - apack's pin moves to this repository's `main`, with EARS on `writeSync` and the host on `backupSync`.
   - The ties above are cut, and the builders, the schema and the error table are typed from `protocol/`.
   - Packs and generated code import through the SDK.
   - The specs under "Written here, not moved" are written beside EARS.

   Done when: EARS' suite passes on the pinned commit with no setup file from another package; a scratch copy of
   this repository with one `Op` field renamed fails apack's `typecheck:ears`; no file under
   `packages/default-setup` or `tests/packs`, and no generated file, names `@apackdev/ears`; and
   `test:packaged-authoring`, with the scaffold no longer adding EARS, installs and builds a pack.
1. **Recount.** The table under What it costs, taken again over phase 0's commits and whatever follows them. If a
   settled EARS still changes mostly with apack's code, the boundary in `design.md` is the right one and this plan
   stops here, with phase 0 kept.
2. **The package arrives here.** The subtree import, recording the apack commit it was taken at; the workspace; the
   two names; the build; CI. Done when: every moved spec passes in the fast tier on its three platforms; three
   scratch copies each fail it (a refusal removed from the engine, a builder writing another op, a watch not run
   again), so the suite is shown able to fail here; the API report differs from apack's last only by the package's
   name and `/database`; the benchmark and the watch stress run from this repository, and `../architecture/ears.md`
   names their commands (the dated records in `../evidence/` are left as they are).
3. **apack switches**, prepared against phase 2's branch so that it merges within the day. Its packages depend on
   the linked `@elmdb/ears`; `packages/apack-ears` and the list under "Goes from apack" are deleted; the pin moves.
   Done when: `git log <the recorded commit>..HEAD -- packages/apack-ears` in apack is empty, or each commit it
   lists has been imported here; apack's unit, integration and acceptance suites pass; and the packaged app, started,
   answers a query through EARS (its native smoke test loads the binding alone and proves nothing of EARS).
4. **The documents**: `design.md`'s boundary, Packages and Vocabulary; `CLAUDE.md`'s Layout and Checks; `README.md`,
   which opens a database and runs a query through EARS; `../architecture/ears.md`, in this repository's vocabulary
   (a database, never a store). Done when `git grep '@apackdev/ears'` here finds it only under `archive/` and
   `evidence/`.

Phases 2 and 3 are one move in two repositories. Nothing under "Opened" starts before 3.

This plan is in no milestone of `../roadmap.md`, and is not ahead of one.

## Unproven

- What a settled EARS changes with. Phase 1 exists to find out.
- That EARS can leave apack's shared-instance list once packs import it only through the SDK. The harness that runs
  a pack's dependencies shares EARS by name. Until a pack built that way is shown loading one copy, EARS stays on
  the list under its new name.
- How npm records a linked package's `msgpackr` in apack's lockfile, and whether a pin move then dirties it. Today's
  entry for the linked binding has no dependencies.
- That apack's API bundle, which inlines EARS from source, inlines it from `dist` with source maps good enough for
  the call sites `calls.ts` reports; and that the renderer's dev server serves a linked `dist` from outside its
  root.
- That every moved spec passes under vitest's deadlines, and on Linux and Windows runners, where CI has never run
  them.
- What EARS' suite adds to a pull request's time here.
- That blame survives the subtree import and the rename to `packages/ears`.
- The registry readings above are one `npm view` each; a name being unregistered is not a name being held.

## What the review changed

An independent reader checked the first draft against both repositories.

- **Changed.** apack's pin is 324 commits behind and EARS doesn't run on `main`: phase 0 now starts there. Read
  answers have no generated type: the claim is withdrawn and the work listed. The port stays structural, since the
  binding's types would have put `Buffer` and `elmdb` into the root entry's declarations. The type work needs no
  move and went to phase 0, and stopped being a reason to move. The cost table was recounted by a stricter rule,
  which the first draft's caption did not match: 31 and 51 of 68, where the draft said 32 and 40 of 73 and "slightly
  more common". Phase 1 is new, because of it. Four conditions that could not fail were replaced (a pack that never
  listed EARS, a smoke test that never loads it, a rename that fits neither a signature nor a table, a phase with
  none). `elmdb` became a peer. The alternatives are weighed. The SDK's exports are listed in full, and the claim
  that its reports already showed most of them is withdrawn. CI's Node, Windows' build and the change filter are
  named.
- **Held.** EARS imports no apack package. The 21 codes, the cast, the two run-time ties and the test setup are as
  described. A pack reaching EARS through the SDK works for backends, frontends and facades. An npm workspace here
  does not disturb apack's link to the binding.
- **Not checked by anyone**: the entries under Unproven.
