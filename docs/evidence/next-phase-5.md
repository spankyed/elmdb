# Phase 5 of goal-elmdb-next: field types from apack's shapes

2026-10-27, apack `session/elmdb-next` over elmdb `f8dc82e` (no elmdb change in this phase: apack starts declaring the
field types elmdb already checks). The plan and its outcome: apack's `docs/archive/plans/field-types-from-shapes.md`.

## The four steps

1. **The mapping.** Codegen reads each shaped field's type with the TypeScript checker (`rowRulesOf`, apack's
   `packages/apack-sdk/src/build/module-exports.ts`) into the pack's `rowRules` (`RowRules.fields`,
   `@apackdev/ears`). Spec: `packages/apack-sdk/tests/build/generate-entries/modules.spec.ts`, "types each field from
   its shape", one property per row of the plan's table — `string`, `number`, `boolean`, a one- and a many-literal
   `enum`, a `ref` to one type and to two, an untagged id as `string`, lists of strings, of readonly numbers and of
   nullable strings (`any`), `| null`, `?`, both, and an object, a record, `unknown`, `Date` and `string | number` as
   `any` — read from a fixture with no generated files on disk.
2. **The schema.** `toElmdbSchema` declares each typed field; `FIELD_TYPE` is an `EarsErrorCode`. Specs:
   `packages/apack-ears/tests/engine/writes.spec.ts`, "fields typed by a type's shape": a wrong value refused with
   `FIELD_TYPE` from `set`, `setMany`, `create` and `upsert`, `null` refused where the shape doesn't allow it, and taken
   where it does; a field outside the shape taken; a `ref` to an undeclared type held as text; a field a text index
   reads held as anything.
3. **The audit**, below.
4. **The writers the compiler can't see**: default-setup's flows, actions and systems, the console's examples and the
   host, through their specs (`npm run spec:full`) and the chain; default-setup declares no migrations. Three findings,
   fixed in the code that wrote them: the notes system cleared a note's saved order by writing `null`, which the shape
   doesn't hold (`noteCommands.update` now removes the field; its spec says so); forking a thread copied a stored `null`
   field across (it copies only non-null values now); and the onboarding action cleared its thread's `forcedMode` with
   `null`, which, refused, stalled onboarding after its project step — the external pack's onboarding E2E
   (`test:external-pack:app`) found it — so `threadCommands.update` removes a field it is given `null` for, no field
   of a thread's shape holding one (`tests/features/threads/be/repository/index.spec.ts`). A scan of every action and
   backend for other `null` field writes found none outside a nullable field or an untyped entity.

**Found building it.** A shape names its ids' types through the pack's `#generated/ears`, which codegen writes only
after reading the shapes and which a clean checkout doesn't have: on a first build every id read as `any`, on a second
as a `ref`. The shape reader is handed the `ears.ts` the run will write (a second TypeScript program over the shape
modules; default-setup's `generate:entries` took 14.2 s with it, 2026-10-27, on a loaded machine). And a pack that
doesn't check nulls has its `| null` erased, so its fields are held nullable rather than refusing `null` its types allow.

## The audit

`check()` over a fresh copy of G8's production-shaped data (`~/Develop/elmdb-next-scratch/prod`, 22,816 entities)
opened under the schema the app declares with default-setup registered, the SDK's and host's types beside it. The copy
is in the backup's own contract, numbered under its prefixes (`T`, `F`, `NOTE`, `PROMPT`; the dump's README), which can't
change once stored, so the audit keeps those four and measures the field types alone. The script is a local one-off,
recorded here rather than committed: it composes `toElmdbSchema` of the SDK's and host's types and default-setup's built
registration, copies `data.mdb`, opens the copy under that schema and runs `check()`.

| Finding | Rows | Cause | Done |
|---|---:|---|---|
| Notes without `hideCompletedChildren`, which the shape requires | 280 | written before `noteCommands.create` wrote the field, which it has since apack `a86972a9d6` (2026-10-23), after the backup (2026-10-17) | not migrated: stored developer data is the user's (apack's `CLAUDE.md`, Backward compatibility); readers default it (`?? false`) |
| Messages holding `forkable: null`, which `forkable?: boolean` doesn't take | 154 | not found in apack's history: no writer ever wrote a literal `null`, and every writer today writes a boolean (the stream consumer's since `6a8d09ccb5`, 2026-05-31), so it predates or came with the backup's conversion | not migrated; forking such a thread no longer copies the `null` |
| Two entities holding one short code | 9 Thread, 8 Note | G8's known duplicates | G8's, unchanged |

No derived differences.
