# The rest of the supply chain

Status: built in part, 2026-11-20 (elmdb#202); the rest waits on the domain. This is what remained of change 9 of the
road to 1.0 (`../../roadmap.md`) once the prebuilt binaries were built (`release.yml`,
`../../architecture/testing.md`, Releases): `cargo-deny` and an audit in CI, and the `$schema` URL resolving. It says
what each one checks, what the tree answered when it was planned, the decisions, and the order to build them in.

- **Built:** `deny.toml` and its checks in `ci.yml`'s repo job and in `cargo xtask pre-push`; the daily
  `advisories.yml`; and the generated JSON Schema's `$id`, which `check-generated` holds
  (`../../architecture/testing.md`, Platforms; The work, in order, steps 2 to 4).
- **Waiting on the domain:** the domain itself, the owner's decision (decision 5); serving the JSON Schema at its
  URL; and the scheduled check that fetches it. `elmdb.dev` isn't registered, so a publish to it or a fetch of it
  would fail on every run, and neither is built. The domain has a deadline: it has to be settled before milestone 0.3
  freezes the protocol (When).

## Where it started

Read on 2026-11-18, at `9c6e765`, before any of it was built:

- **Licenses already have one policy.** `crates/elmdb-node/about.toml` lists the licenses a dependency of the binding
  may be under. Nine crates have clarifications, which name what a crate's declared license leaves out. Three of those
  crates also have a license accepted for them alone: `lmdb-master-sys`, which compiles LMDB under OLDAP-2.8, with code
  under ISC; `futures-channel`, under BSD-2-Clause; and `steppe`, whose license is given only as a WTFPL file.
  `cargo xtask check-notices` fails a pull request on a dependency outside it (`ci.yml`, the repo job). It reads the
  binding's normal dependencies under its default features, which is what the `.node` files are built from; nothing
  else ships.
- **The dependency tree has repository checks of its own:**
  - exactly one LMDB `-sys` crate (`check-one-lmdb`);
  - the ICU crates, and every crate whose output an index stores, at exactly their locked version (`check-exact`);
  - what the `diagnostics` feature and Leiden's parallelism may pull in (`check-diagnostics-off`,
    `check-leiden-serial`).
- **Nothing reads the RustSec advisory database**, and no workflow runs `cargo-deny` or `cargo-audit`.
- **`Cargo.lock` holds 328 packages:** the workspace's four, and 324 from crates.io. No git source and no other
  registry.
- **The schema language is a URL nobody owns.**
  - `SCHEMA_LANGUAGE` is `https://elmdb.dev/schema/1.json` (`crates/elmdb/src/schema/model.rs`). A schema naming any
    other is refused (`schema/parse.rs`), at `open`, at `setSchema` and when a dump is loaded.
  - A database given a schema stores it, URL included, in its canonical schema (`m/schema`) and its hash.
  - The literal URL is also written in the README, `docs/architecture/schema.md`, about twenty of the binding's tests
    and benches, and `scripts/measure-recovery.mjs` and `scripts/release-smoke.mjs`. EARS names it in
    `ELMDB_SCHEMA_LANGUAGE` (`packages/ears/src/schema.ts`), and apack in `build/prod/smoke-native.mjs`.
  - `elmdb.dev` has no DNS record, and Google Registry's RDAP answers 404 for it: it isn't registered.
  - The JSON Schema meant to be served there is generated, `crates/elmdb-node/protocol/schema-1.json`
    (`cargo xtask generate`). It has no `$id`. GitHub Pages isn't enabled for the repository.

## What `cargo-deny` found

One run of cargo-deny 0.21.0 on a scratch copy of `origin/main` at `c4f73b6`, 2026-11-18, whose `Cargo.lock` and
`about.toml` are `9c6e765`'s. Its configuration had:

- the seven targets of `package.json`'s `napi.targets`;
- as the allowed licenses, `about.toml`'s `accepted` list and OLDAP-2.8;
- `multiple-versions` set to warn;
- unknown registries and git sources denied.

It answered:

- **advisories: one failure.** `bincode` 1.3.3 is marked unmaintained (RUSTSEC-2025-0141): its authors stopped
  development and call 1.3.3 complete. No vulnerability is reported against it, and no version is patched. It arrived
  through `heed-types` 0.21, by heed's default `serde-bincode` feature, which elmdb's own `heed` dependency turned on
  and nothing used. Since 2026-11-19 elmdb takes heed without its default features, and `bincode` is in no build, so
  this failure is gone without an ignore.
- **licenses: one failure.** `steppe` 0.4.1, through hannoy, declares no license field, and cargo-deny reads its
  `LICENSE` file as WTFPL, which the list didn't allow. It also warned that OLDAP-2.8 was never seen: it reads
  `lmdb-master-sys` as its declared Apache-2.0, and `futures-channel` as its declared `MIT OR Apache-2.0`, without the
  clarifications `about.toml` makes.
- **bans: passes, with warnings.** Eight crates are built at more than one version: `rand`, `rand_core` and
  `getrandom` at three, and `rand_chacha`, `hashbrown`, `foldhash`, `syn` and `convert_case` at two.
- **sources: passes.**

With the `deny.toml` that is built, cargo-deny 0.21.0 on 2026-11-20 at `09ec765`, macOS arm64, answered
`advisories ok, bans ok, sources ok`, with the same eight crates warned of as built at more than one version.
`npm audit --audit-level=high` found nothing the same day.

## Decisions

1. **`cargo-deny` checks advisories, bans and sources, and not licenses.**
   - `about.toml` stays the one license policy. It already fails a pull request on a license it doesn't accept, and
     it holds the clarifications the notices are written from.
   - cargo-deny can hold clarifications too (`[[licenses.clarify]]`, with file hashes). But that would be a second
     copy of all nine, which would drift from the first.
   - With no `[licenses]` section, a bare `cargo deny check` rejects every crate. So `deny.toml`'s comment, the CI
     step and `pre-push` always name the checks they run.
2. **The audit is `cargo deny check advisories`, against RustSec.** `cargo-audit` reads the same database, so running
   both would add a second tool and a second list of exceptions for nothing.
   - The npm package depends on nothing, so the npm side has no runtime dependency to audit.
   - Its build tooling, `@napi-rs/cli` and its cross toolchain, comes through the root `package-lock.json`. Its audit is
     `npm audit --audit-level=high`, which gates nothing and runs with the daily run below. It found nothing on
     2026-11-18.
3. **Advisories are read each day on `main`, and on a pull request that changes the lockfile, as a merge gate.**
   - An advisory appears without any change to the repository. If every pull request checked advisories, a new one
     would fail the next unrelated pull request. If no pull request checked them, a vulnerable crate added by a pull
     request would merge.
   - So the pull request's check has to gate the merge. A workflow filtered by path can't be a required check: when it
     doesn't run, the check stays pending.
   - The check runs instead in `ci.yml`'s repo job, which `ci-pass` waits on. It runs when the `changes` job reports
     that `Cargo.lock` or `deny.toml` changed, since a pull request adding an ignore should meet the check it relaxes.
   - Bans and sources run in the same job on every pull request, beside `check-notices`.
   - The daily run on `main` is a workflow of its own, `advisories.yml`, which gates nothing and fails like
     `lmdb-upstream.yml` does, with `contents: read`.
4. **Every exception names its advisory or crate and why.** An advisory is answered by removing the crate where
   that can be done, as `bincode` was, and by an ignore entry naming the advisory and why only where it can't.
   cargo-deny warns when an ignore no longer matches anything (`advisory-not-detected`), so no entry outlives its crate
   unnoticed. `unmaintained = "workspace"` is not used: it would hide a transitive crate's advisory without saying why.
   - Duplicate versions warn and don't fail until the eight above are gone or each is excused, since most come from
     dependencies' own pins.
5. **The schema language moves under a domain the owner holds, before 0.3.** Two ways, the first recommended.
   - **Register `elmdb.dev`**, the owner's purchase, and serve the repository's GitHub Pages there, with `schema/1.json`
     the generated `schema-1.json`. The URL every schema, database, dump and apack already names stays as it is, and
     nothing in the code changes.
   - **Or change `SCHEMA_LANGUAGE`** to a URL the owner already holds, such as
     `https://spankyed.github.io/elmdb/schema/1.json`. It ties the protocol to an account name, and it costs more than
     the constant:
     - every file above that writes the URL, and apack's two, change with it;
     - a dump already made fails to load, since a load refuses a schema naming the old URL. So does a dump the new build
       makes of a database whose stored schema still names it, such as one opened without a schema or read-only. The
       load would have to accept the old URL and write the new one;
     - a stored schema is read without checking its URL (`Schema::from_canonical`), but its hash holds it. So a
       writable open given a schema under the new URL writes it, with no change listed (`diff` doesn't compare the
       URL), and other processes' handles must reopen (`SCHEMA_CHANGED`). A read-only open given one is refused
       (`SCHEMA_CHANGE_REFUSED`) until a writable open has written it.

     That is read from the code (`database.rs`, `schema/diff.rs`, `dump.rs`), not tested; the change would come with
     the tests. Format 1 is still edited in place until a release has users (`../format-freeze.md`), so this is
     allowed now and not after.

   Until one of them is done, anyone can register `elmdb.dev` and serve what they like at the URL an editor fetches
   to complete a schema written as JSON.

## The work, in order

1. **The domain** (owner): register `elmdb.dev`, or choose the second way above. Not decided. Serving the schema
   (step 4) waits on it, and so does 0.3.
2. **`deny.toml` and the checks a merge waits on.** Built, 2026-11-20 (elmdb#202).
   - `deny.toml` at the root:
     - `[graph]` targets from `napi.targets`;
     - `[advisories]`, with no ignore today;
     - `[bans]` with `multiple-versions = "warn"`;
     - `[sources]` denying unknown registries and git sources;
     - no `[licenses]`, and a comment saying `about.toml` owns them and that a bare `cargo deny check` fails.
   - In `ci.yml`'s repo job, at a pinned cargo-deny: `cargo deny check bans sources`, and
     `cargo deny check advisories` when the `changes` job reports `Cargo.lock` or `deny.toml` changed.
   - `cargo xtask pre-push` runs both, so a push meets them before CI does. Where cargo-deny isn't installed it
     refuses, with the command that installs it.
   - `docs/architecture/testing.md`, Platforms, names them.
3. **The daily run**, `.github/workflows/advisories.yml`, built, 2026-11-20 (elmdb#202): `cargo deny check advisories` and
   `npm audit --audit-level=high` on `main`. It gates nothing and fails on its own run, which testing.md's list of
   workflows that gate no pull request gains.
4. **The schema served and held to the code.**
   - Built, 2026-11-20 (elmdb#202): `cargo xtask generate` writes `$id` into `schema-1.json`, equal to
     `SCHEMA_LANGUAGE`, and `check-generated` holds it there.
   - Waiting on the domain: Pages publishes `crates/elmdb-node/protocol/schema-1.json` at `schema/1.json` on each
     push to `main` that changes it. GitHub Pages isn't enabled for the repository.
   - Waiting on the domain: a scheduled check fetches the URL. It fails when the answer isn't 200, when its type doesn't start with
     `application/json` (Pages adds a charset), or when the bytes differ from the committed file.
5. **The roadmap's item 9 is marked built**, each gate with the run or check that holds it. It names steps 2 and 3
   and the `$id` as built, and the served schema as waiting; it is marked built once the URL resolves.

## How each is shown to fail

Each gate fails once on a mutated copy, never the working tree (`CLAUDE.md`). The first three and the `$id` were
shown on 2026-11-20, on copies of `09ec765` with the change, with cargo-deny 0.21.0 on macOS arm64; elmdb#202's
description holds what each printed:

- **Sources:** a copy whose crate's `Cargo.toml` takes a dependency from a git repository. A git source written into
  `Cargo.lock` alone doesn't do it: cargo refuses the lockfile with `--locked`, and prunes the entry without it, so
  the sources check never sees one. Shown: `elmdb-cli` given `itoa` from its git repository fails
  `cargo deny check sources` with `source-not-allowed`.
- **Bans:** a copy that bans a crate the tree builds. Shown: `deny.toml` given a ban on `heed` fails
  `cargo deny check bans` with `banned`.
- **Advisories:** a copy that takes heed with its default features again, which fails on RUSTSEC-2025-0141 as the
  run above did. Shown: it fails `cargo deny check advisories` on `bincode` 1.3.3, with that advisory.
- **The `$id`:** a copy whose committed `schema-1.json` has another `$id`, and a copy whose generator writes another.
  Shown: `cargo xtask check-generated` fails on each with `schema-1.json: changed`, and the generator's own test
  fails on the second.
- **The served schema**, not built: a copy of the check pointed at a file that differs by a byte.

## When

- **The domain:** before milestone 0.3, which freezes the protocol. After it, changing the URL is a protocol break.
- **`deny.toml` and the daily run:** before the first tag, so the first release's dependencies have been read against
  RustSec. Built on 2026-11-20, and no release is tagged.
- **Serving the schema:** once the domain is settled.

## Not covered here

- Signing releases, and npm provenance, which need publishing to npm first (`release.yml` publishes nothing).
- Pinning the release workflow's container images and Node download to digests (the release's own review).
