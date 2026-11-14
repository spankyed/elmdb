# EARS in elmdb's repository

Status: planned, 2026-11-14; nothing is built. This is the third draft: two independent reads, each against both
repositories, changed the two before it (What the reviews changed, at the end).

EARS, the entity-attribute-relation system, is the fluent TypeScript API over an elmdb database: what a program
holds (`createEarsEngine({ database })`), its query and write builders, which compile to plans and batches, its
watches, and its typed facades. It is apack's `@apackdev/ears` today: `packages/apack-ears` in `spankyed/apack`,
3,873 lines under `src/` and 4,521 under `tests/` in 20 spec files (`wc -l` and `grep -c '\.spec\.ts$'` over
`git ls-files` at apack's `3d5f18867a`). This plan moves all of it here, as a package of its own beside the binding,
and says what apack keeps.

In this document "the engine" is the Rust crate, as everywhere in this repository. apack calls EARS an engine too
(`createEarsEngine`); that name is one of the decisions left for after the move.

`../architecture/design.md` (The boundary with apack) decided the opposite on 2026-10-25, and named what would
reverse it: "If the builders settle and another program wants them, they move into elmdb then." Neither condition is
met. The move was decided by the repositories' owner on 2026-11-14 for the reasons below; this plan counts what it
costs rather than re-deciding it, and that section of `design.md` is rewritten in phase 3.

## Why move it

- **The builders are the only way anyone uses elmdb from JavaScript.** The binding takes msgpack plans and batches,
  which nobody writes by hand. A database whose usable API lives in its one user's repository has none of its own:
  the README can't show a query, and a second program can't start.
- **EARS' specs are the widest test of the engine from outside, and this repository's CI doesn't run them**: the
  black-box specs, a reference model driven by random workloads with a watch on every read it checks, and the watch
  contract, 16 spec files under `tests/engine/`. They run in apack, in a macOS job, after an engine change has merged
  here and apack's pin has moved to it. The pin moved 85 times between 2026-10-25 and 2026-11-06 and not since
  (`git log --no-merges --since=2026-10-25 -- vendor/elmdb.rev` in apack). apack's CI runs commit `4b67670`, 252
  commits behind `bf4b21b` (`git rev-list --count --no-merges 4b67670..bf4b21b`), and one of them (`6616fd3`,
  2026-11-12) made the binding's `write` answer a promise. EARS calls `write` and reads its answer at once, so EARS
  as it stands does not run on this repository's `main`, and nothing here says so.
- **A protocol change and the builder that uses it become one pull request**, tested together before either merges.

Typing EARS' builders from the binding's generated types is not a reason to move: it can be done where EARS is now,
and is phase 0.

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

31 of 68 changes crossed repositories; 51 would have. **On the only data there is, the move makes a change that
crosses repositories more common, not less.** That data is twelve days of building EARS onto elmdb, when every
change touched everything, and EARS' source has not changed since; it is also `design.md`'s own reason for the
boundary it drew, that the builders change with apack's vocabulary. What the move buys for that cost is which half
of a change is whole: today an engine change merges here untested by the API that uses it; afterwards the engine and
its API merge together, tested on three platforms, and apack's half is the SDK and host code that uses them.

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

- **Type EARS from `protocol/` where it is, and stop.** Gets the compiler's checks, and leaves the reasons above
  unanswered. It is phase 0, worth having either way.
- **A job here that runs apack's EARS suite against the pull request's binding.** A pull request that changes the
  protocol fails it until apack's EARS follows, which can't merge until the pull request does: the job could only
  ever be advisory, and this repository's CI would read another repository's head.
- **Move the port, the codec and the compilers; leave the typed contract in apack.** `runtime.ts` is the builders'
  interface: it would split each builder from its type across two repositories.
- **A subpath of the binding's package (`elmdb/ears`).** A mismatched pair becomes impossible by construction, but
  the binding stops being napi-rs's generated CommonJS loader with no dependency, in a crate's directory; an exact
  peer range (below) gets the same guarantee at publication.

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
`published-sdk-types` and `published-sdk-any` (the packed package, with `elmdb` installed beside it, compiles for a
consumer with no Node types, on TypeScript 5.7 and the current one); and the rule apack's import guard holds inside
EARS' source (`ELMDB_RULES`' second entry), reworded as below.

**Stays in apack**: the bound database and the operations over it (`@apackdev/sdk/ears`), the SDK's `EARS`
namespace with its entity names, codegen, the schema assembled from pack manifests, the Database console, backup,
and their specs. The pack rule that a pack loads no database (`elmdb-imports`) stays, with the new names.

**Changes in apack**:

- `tests/acceptance/packaged-authoring/test-packaged-authoring-author.sh` packs `vendor/elmdb/packages/ears` where
  it packs `apack-ears/publish` now.
- `electron-builder.mjs` excludes the linked package's `src`, `tests`, `bench` and `etc`, as it excludes the
  binding's crate source now.
- The SDK declares `elmdb` as a peer, since its `/testing` entry opens databases and its declarations will name the
  binding's types through EARS' (below).

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
in-process. `@elmdb/ears`, `elmdb-ears` and `elmdb` answered 404 (same reading); a name being unregistered is not a
name being held, and registering the scope is the owner's, before anything is published. `elmdb-ears` is the name
if it can't be had. It is not under `crates/`, which holds crates; `design.md`'s Packages table, whose rule is one
product name with the host language as the suffix, gains a row and a sentence for a package that is no binding.

```
crates/elmdb, crates/elmdb-node (npm `elmdb`), crates/elmdb-cli, xtask
packages/ears/            npm `@elmdb/ears`, private, 0.0.0, as the binding is
  src/  tests/  bench/  etc/  scripts/dist-current.mjs  package.json  tsconfig*.json  vitest.config.ts
package.json              private; workspaces: crates/elmdb-node, packages/ears; all tooling
```

From the root: `npm ci`, `npm run build` (the binding, then EARS' `dist`), `npm run typecheck`, `npm test` (the
binding's `node --test`, then EARS' vitest), which `cargo xtask pre-push` already runs; `pre-push` also runs `npm ci`
when the lockfile is newer than `node_modules`, since a moved dependency would otherwise test the old one.
`.gitignore` gains `packages/ears/dist/`. Nothing is published: this repository's rule stands, and publishing
`elmdb` and `@elmdb/ears` together is a decision of its own, which apack's SDK already waits on (its published
manifest names `elmdb@0.0.0`, which the registry doesn't hold).

**CI gets an EARS job of its own** rather than EARS joining the binding job. The binding job's Node is 23, which
vitest 4.1.12 doesn't support (`^20 || ^22 || >=24`), and moving it would move the read and SQLite benchmarks in the
same step, which were read on 23. The EARS job builds the binding and `dist`, then runs typecheck, specs, the
consumer compile and the API report check, on Node 24, in the fast tier's three platforms (on Windows by cargo, as
the binding is built there, then `tsc`). It does not run under Electron or on each Node the platform job reads, nor
in the full tier's musl container: EARS has no native code, and a platform difference reaches it only through the
binding, which those jobs already run. The change filter, which reads any `.md` as a document in `ci.yml`,
`full.yml` and `mutants.yml`, stops reading `packages/ears/etc/*.api.md` as one in all three. An npm workspace at
the root leaves apack's link to `crates/elmdb-node` as it is, the generated-files check and the committed loader's
diff check name their paths and are unaffected.

**`elmdb` is EARS' peer, at exactly the version it was built with.** One process holds one binding (`design.md`,
principle 5), and a peer is installed once, beside its dependents, where a dependency may be installed twice. EARS'
`elmdb` and the app's then resolve to one file: from EARS' real path in a checkout, `elmdb` is the workspace's link
to `crates/elmdb-node`, the same file apack's `node_modules/elmdb` links to. That holds only after `npm ci` has run
in the checkout with the workspace in place, which apack's link checks (below). While both are `0.0.0` the range is
`0.0.0`; at publication both take one version and the range stays exact, so no mismatched pair installs.
`msgpackr` is EARS' one dependency; its manifest holds no tooling. npm records a linked package's manifest in
the consumer's lockfile (apack's `vendor/apack-terminal/view` is recorded so), so EARS' `msgpackr` and its peer
appear in apack's `package-lock.json`, and a pin move that changes EARS' manifest changes apack's lockfile with it.

**Two entries, as now, one renamed.** `@elmdb/ears` is the builders and their types: it loads no native code and no
Node module, so a browser bundle can hold its constants and helpers, as apack's frontends do. `@elmdb/ears/database`
is today's `/lmdb` (which opens an elmdb database, not an LMDB one): `openDatabase`, `createTemporaryDatabases` and
`DATABASE_FORMAT`. **The root entry imports types from `elmdb`'s generated protocol, and nothing else of it.** That
is what typing the builders means, and it puts `import type … from 'elmdb/protocol/…'` into EARS' declarations, so
a consumer compiling against them has `elmdb` installed, which a peer guarantees. The rule it replaces said the root
entry imports nothing of `elmdb`; the rule that holds is that it imports no value of it and no Node module, and its
declarations name no `Buffer`. Type imports erase, so a frontend bundle is unchanged.

**The port stays structural, and is checked member by member.** `EarsDatabase` remains EARS' own statement of what
it needs, in `Uint8Array`s, which a spec can wrap with an object of its own; `schema`, `setSchema`, `stats` and
`SchemaChange` take the generated types where they are wider now. Assignment alone checks too little: any return
type is assignable to `void`, so a member turning from synchronous to a promise compiles (`backup` did, in
`6616fd3`). `src/database/port.test-d.ts` asserts for each member that its return type is exactly the binding's,
`Buffer` read as `Uint8Array`, and `openDatabase` returns the binding's `Database` with no `as`.

**The builders are typed at their literals, and the casts are counted.** `write.ts` builds an op without its `id`
and spreads one in; `query.ts` adds `depth` by a computed key and passes search options through as a record. Each is
restructured so that an op or a step is a literal of the generated type, and none is made with `as`. A required
field renamed in Rust then fails the build at every site; an optional field renamed is caught where it is written as
a literal and not where it is spread, which is why the spreads go.

**Every export keeps its name and meaning through the move.** Nothing is renamed, so the moved specs are the proof
that nothing changed, and apack's change is the package's name, which 173 files outside the package name today
(`git grep -l '@apackdev/ears'` at `3d5f18867a`, documents and the lockfile aside).

**A pack names only apack's SDK.** Today a pack depends on `@apackdev/ears` by name: `apack init` scaffolds it, the
generated `#generated/ears` and `#generated/services` import it, and default-setup imports it in four source files.
The SDK exports what those use. Generated code takes `EntityId`, `AttrKind` and the rest from the `EARS` namespace
the SDK already exports, which aliases the same types; `@apackdev/sdk/ears`, which generated code already imports,
gains `defineEars`, `EarsSchema`, `UntypedSchema`, `ShapeOf`, `ShortCode`, `BaseEntity`, `QueryBuilder`,
`WriteBuilder`, `RelationRow`, `RelationMatch`, `RelationStats`, `QueryExplanation` and `RESERVED_ATTRIBUTES`. A
pack then has one dependency whose version the app decides. Whether EARS can then leave apack's shared-instance list
is not shown (Unproven).

**apack consumes it as it consumes the binding**: `"@elmdb/ears": "file:../../vendor/elmdb/packages/ears"`, through
the same link and the one pin. It consumes `dist`; the `@apackdev/source` condition is apack's and goes. A `dist`
is a build output in a checkout whose branch changes, so **this repository owns the check that it is current**:
`packages/ears/scripts/dist-current.mjs` writes a stamp after a build succeeds (the tree id of `packages/ears` at
`HEAD`, whether `git status -- packages/ears` was clean, and the TypeScript version), and answers whether the stamp
matches the checkout now. apack's link script calls it and builds `dist` when it doesn't match, and checks that the
checkout's `node_modules/elmdb` is the workspace's link. One script in one repository, so the two can't disagree.

**The pin names a tested pair in CI, not locally.** apack's CI checks out the pinned commit. A developer's link
points at their checkout, on whatever branch is there, held only to having the pin in its history: apack's link at
the time of writing points 123 commits past the pin, at a binding built before `6616fd3`. That stays as it is, with
the warning the link already gives when the checkout differs from the pin, widened to `packages/ears`.

**History comes with it.** `git subtree split -P packages/apack-ears` makes the package's history its own, which
rewrites the commits and drops the empty merges (the package's log holds 225 commits, merges included, 213
without: `git log [--no-merges] --oneline -- packages/apack-ears`; the split's count will differ), and it is merged
here by a merge commit, never squashed, so `git blame` survives. Both repositories are public (`gh repo view`,
2026-11-14), so nothing is published that wasn't. The commits keep their messages; an `#N` in one will link to this
repository's issue of that number, which is wrong and left. The package's licence goes from MIT to this
repository's `MIT OR Apache-2.0`, which is its one author's to do.

## What it unlocks

From phase 2, held by CI: every pull request here runs EARS' specs and model harness on Linux, Apple silicon and
Windows before it merges (they have only ever run in CI on macOS). No plan step exists without a builder, and no
builder waits on a pin. The benchmarks sit beside the records that cite them. The README opens a database and runs
a query.

From phase 0, held by the compiler, and had without the move: an op or a step is a literal of a generated type, so a
required field renamed in Rust fails every builder that writes it; `toElmdbSchema` returns the generated `Schema`;
the error table is checked against the generated `ErrorCode`, so a new refusal is mapped or listed as passed
through; the port's members are held to the binding's, return type by return type.

Not had, and not promised: a read's answers have no generated type. The engine writes each of its 19 kinds by hand
(`grep -o 'w.kind("[a-z_]*")' crates/elmdb/src/engine/reads.rs | sort -u`, at `bf4b21b`), and a write's envelope
(`results`, `changes`, `sequence`) is described only in a comment. Types for them are new work in the engine.

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
   - **The pin moves to this repository's `main`.** That is more than a rename. EARS calls `writeSync` where it
     calls `write` (the host's `backup` already awaits its promise and stays). Between the pin and `main`: a change
     set's `created` and `destroyed` mean something else for an id destroyed and made again, and `Changed.destroyed`
     lists each id once, which the SDK's projections read; a relation change carries `movedFrom`; `bulkLoad`
     refuses a value outside the value spec, which a restore of an old backup can hold; `PANICKED` is a new code;
     text ranking was rebuilt, which moves hit order and scores; `check` and `rebuild` answer promises.
   - **The ties above are cut.**
   - **The builders, the schema, the error table and the port are typed** as Decisions says, and the import guard's
     rule inside EARS is reworded to allow type imports from `elmdb/protocol`.
   - **Packs and generated code import through the SDK.**
   - **The specs under "Written here, not moved" are written beside EARS.**

   Done when:
   - EARS', the SDK's, the host's and default-setup's suites pass on the new pin, with EARS using no setup file
     from another package;
   - a backup taken by the old pin is restored by the new one;
   - scratch copies of this repository fail apack's `typecheck:ears`, one each for a renamed required field of
     `Op`, `Step`, `Terminal` and `Schema`, a code added to `ErrorCode`, and a port member turned into a promise;
   - `git grep -n ' as ' -- packages/apack-ears/src/write.ts packages/apack-ears/src/query.ts` names no op or step;
   - the import guard passes;
   - no file under `packages/default-setup` or `tests/packs`, and no generated file, names `@apackdev/ears`;
   - `facade-typing.integration.spec.ts` passes, since its completions are what moving generated code onto the
     SDK's namespace could lose;
   - `test:packaged-authoring`, with the scaffold no longer adding EARS, installs and builds a pack.
1. **The package arrives here.** The split and merge, recording the apack commit it was taken at; the workspace; the
   two names; `dist-current.mjs`; the build; the CI job. Done when:
   - every moved spec passes in the EARS job on its three platforms;
   - three scratch copies each fail it (a refusal removed from the engine, a builder writing another op, a watch not
     run again), so the suite is shown able to fail here;
   - the API report differs from apack's at the recorded commit only by the package's name, the entry `/database`,
     and the `elmdb/protocol` import paths;
   - the benchmark and the watch stress run from this repository, and `../architecture/ears.md` names their commands
     (the dated records in `../evidence/` are left as they are).
2. **apack switches**, prepared against phase 1's branch so that it merges within the day. Its packages depend on
   the linked `@elmdb/ears`; the changes under "Changes in apack" are made; `packages/apack-ears` and the list under
   "Goes from apack" are deleted; the pin moves. Done when:
   - `git log <the recorded commit>..<the deletion's parent> -- packages/apack-ears` in apack is empty, or each
     commit it lists has been brought here;
   - apack's unit, integration and acceptance suites pass;
   - the packaged app's native smoke test (`build/prod/smoke-native.mjs`), which loads the binding alone today,
     also opens a database and answers a query through the EARS the packaged API loads, and fails on a scratch build
     with EARS' `msgpackr` left out.
3. **The documents.** `design.md`'s boundary, Packages and Vocabulary; `engine.md`, `testing.md` and
   `background.md` where they name apack's package; `CLAUDE.md`'s Layout and Checks; `README.md`, which opens a
   database and runs a query through EARS; `../architecture/ears.md`, in this repository's vocabulary (a database,
   never a store); and this plan, to `../archive/roadmap/`. Done when `git grep '@apackdev/ears'` here finds it only
   under `archive/` and `evidence/`.

Phases 1 and 2 are one move in two repositories, and the openings listed above start after 2, each on
its own reading. From phase 1's merge until phase 2's, EARS is changed here only;
phase 2's first condition is what catches a change made in apack meanwhile.

This plan is in no milestone of `../roadmap.md`, and is not ahead of one.

## Unproven

- That EARS can leave apack's shared-instance list once packs import it only through the SDK. The harness that runs
  a pack's dependencies shares EARS by name. Until a pack built that way is shown loading one copy, EARS stays on
  the list under its new name.
- That apack's API bundle and its CLI bundle, which inline EARS from source today, inline a linked `dist`, with
  source maps good enough for the call sites `calls.ts` reports; and that the renderer's dev server serves a linked
  `dist` from outside its root.
- That every moved spec passes under vitest's deadlines, and on Linux and Windows runners, where CI has never run
  them.
- What the EARS job adds to a pull request's time here.
- That blame survives the split and the rename to `packages/ears`.
- How `mutants.yml` behaves on a change to EARS alone, beyond passing its filter.

## What the reviews changed

Two independent readers checked the drafts against both repositories. The third draft has had no outside read.

- **The first read** found that EARS doesn't run on this repository's `main`; that read answers have no generated
  type; that the type work needs no move; that the cost table's caption didn't match its counts, and recounted it to
  31 and 51 of 68; four conditions that could not fail; and missing CI changes. It also confirmed that EARS imports
  no apack package and that packs can reach EARS through the SDK.
- **The second read** found:
  - that typing the builders from the protocol breaks the rule that the root entry names nothing of `elmdb`, so the
    rule is now restated and the guard named in the done-when;
  - that a cast-free port catches only narrowing, not a member turning into a promise, so the port is held return
    type by return type;
  - that the recount phase had no threshold or sample and contradicted a decided move, so it is gone;
  - that the pin move is more than two renames, and the host's `backup` needs no change, so the changes are listed
    and the done-when covers the suites and a restore;
  - that a digest of `src/` proves less than apack's own stamp, so this repository owns one script both use, and
    "a tested pair" is claimed for CI only;
  - that the peer's stated reason was wrong (npm installs a missing peer too), so the reason is now deduplication,
    the range is exact, and the SDK declares the peer;
  - that the history needs a split and a merge commit, and phase 2's log condition could never be empty;
  - that no test runs a query in the packaged app, so one is named;
  - that builders spread ops, so they are typed at their literals and their casts counted;
  - that the CI change filter lives in three workflows, the binding job's benchmarks depend on Node 23, and the
    musl container runs `npm test`, so EARS gets a job of its own;
  - that an Unproven entry was answered by apack's own lockfile, so it is now a fact;
  - and that four figures lacked their commands, and one sentence counted spec files twice.
- **Held through both**: the 21 codes, the cast, the two run-time ties and the test setup are as described; the SDK
  export list covers every name packs and generated code use, and adding it brings no new module into a frontend
  and no cycle; an npm workspace here leaves apack's link to the binding alone; the document uses no banned word
  and means the Rust crate by "the engine".
