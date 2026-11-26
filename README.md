<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="docs/assets/banner-dark.svg">
    <img src="docs/assets/banner-light.svg" width="600" alt="elmdb">
  </picture>
  <br>
  <em>Say "LMDB" out loud: that's the name.</em>
</p>

An embedded database for Node.js and Electron, with search, vectors, graphs and live queries built in. It runs inside your app with no server, so you don't need a separate search index or custom cache invalidation. Written in Rust directly on LMDB.

- **Zero-dependency wire protocol:** Atomic `elmdb-wire` (`EWB2`) binary write batches in a single FFI crossing — no MsgPack or JSON serialization overhead on writes.
- **Unified search & ranking:** BM25F full-text search (with WAND/block-max), trigram/Levenshtein fuzzy & regex matching, native LMDB-backed HNSW vector search, and hybrid RRF / linear score fusion.
- **Graphs:** Multi-hop BFS, shortest paths, Personalized PageRank, Leiden communities, SCC, topological sort, Jaccard & Adamic-Adar similarity.
- **Sans-I/O live queries:** Coarse-to-fine read-dependency tracking, 128-bit declarative deduplication, adaptive throttling, and cross-process selective catch-up — with zero hidden background timer threads.
- **Enforced schemas & O(1) evolution:** Typed entities, relations with referential integrity (`restrict`, `setNull`, `allowDangling`), O(1) type/field/relation renames, and archived type lifecycle.
- **Repairable derived state:** Every index is a deterministic projection of primary state; `check()` proves equivalence and `rebuild()` swaps cleanly via ping-pong LMDB sub-DB slots.

## Example (`elmdb`)

```js
import { Database } from 'elmdb';
import { buildWireBatch } from 'elmdb/client';

const db = Database.open('./notes', {
  schema: {
    $schema: 'https://elmdb.dev/schema/2.json',
    types: {
      Note: {
        fields: { title: { type: 'string' } },
        indexes: {
          text: { kind: 'text', fields: [['title', 1.0]] },
          rank: { kind: 'rank', relations: ['refs'] },
        },
      },
    },
    relations: {
      refs: {
        cardinality: 'manyToMany',
        endpoints: [{ source: 'Note', target: 'Note' }],
      },
    },
  },
});

db.executeWireBatch(buildWireBatch((b) => {
  const a = b.create('Note', { title: 'Choosing a database' }, 'Note-a');
  const n2 = b.create('Note', { title: 'Backups' }, 'Note-b');
  const n3 = b.create('Note', { title: 'Database migrations' }, 'Note-c');
  b.link(n2, 'refs', a);
  b.link(n3, 'refs', a);
}));

const found = db.query({
  source: { kind: 'type', typeName: 'Note' },
  steps: [{ kind: 'score', spec: { kind: 'bm25f', index: 'text', query: 'database', topK: 10 } }],
  terminal: { kind: 'hits' },
});
console.log(found.hits);

db.close();
```

## Through `@elmdb/ears`

`@elmdb/ears` is the zero-dependency, strongly-typed fluent TypeScript facade over `elmdb`: immutable algebraic `QueryBuilder`, atomic `batch(...)`, engine-native `beforeCommit` savepoint hooks, clock-driven sans-I/O live watches, and compile-time typed facades via `defineEars`.

```ts
import { createEarsEngine } from '@elmdb/ears';
import { openDatabase } from '@elmdb/ears/database';

const database = openDatabase({
  path: './notes',
  schema: {
    types: {
      Note: {
        fields: { title: { type: 'string' } },
        indexes: { text: { kind: 'text', fields: [['title', 1.0]] } },
      },
    },
    relations: {
      refs: {
        cardinality: 'manyToMany',
        endpoints: [{ source: 'Note', target: 'Note' }],
      },
    },
  },
});

const ears = createEarsEngine({ database });

ears.batch((b) => {
  const choosing = b.create('Note', { title: 'Choosing a database' }, 'note-1');
  const backups = b.create('Note', { title: 'Backups' }, 'note-2');
  const migrations = b.create('Note', { title: 'Database migrations' }, 'note-3');
  b.link(backups, 'refs', choosing);
  b.link(migrations, 'refs', choosing);
});

const hits = ears.query.of('Note').search('text', 'database', { topK: 10 }).hits();
const incoming = ears.query.ids(['note-1']).hops({ relKinds: ['refs'], dir: 'in', maxHops: 1 }).all();

ears.admin.close();
```

## Status & Platform Support

Both `elmdb` and `@elmdb/ears` are built from source in this workspace (not yet published to npm).

```sh
git clone https://github.com/spankyed/elmdb.git && cd elmdb
npm ci && npm run build
```

| | |
|---|---|
| Node | 22.17 or later, and Electron through N-API (ABI-stable across upgrades) |
| Platforms | macOS, Linux (glibc and musl) and Windows on x64; macOS and Linux on arm64 |
| Not in scope | a network server, replication, or access control |

## Durability & Concurrency

- **MVCC snapshots:** Readers never block writers; `queryAsync`, `checkAsync`, `rebuildAsync`, `backupAsync`, and `flushAsync` run on Node's worker pool without blocking the event loop.
- **Verified crash-only durability (`synced.v2`):** Choose `durability: 'strict'` (default, fsync every commit), `'grouped'` (periodic group commit with verified rollback detection via `openReport()` and `acknowledgeRestore()`), or `'unsafe'` (ephemeral/tests).
- **Deterministic Simulation Testing (DST):** The entire engine runs against `elmdb-sim`'s fault-injecting storage substrate to prove linearizability, crash recovery, index equivalence, and zero-false-negative watch invalidation.

## Docs

- [`docs/architecture/v2-design.md`](docs/architecture/v2-design.md): authoritative elmdb v2 architecture, storage layout, algorithms, wire format, and verification strategy.
- [`docs/README.md`](docs/README.md): full documentation map.
- [`CLAUDE.md`](CLAUDE.md): workspace layout, invariants, and developer checks.

## Develop

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
npm run build && npm run typecheck && npm test
npm run api:check -w @elmdb/ears
cargo run -p elmdb-cli -- inspect DIR
```

## License

Dual-licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE). LMDB is licensed under the [OpenLDAP Public License](LICENSE-OPENLDAP). Contributions follow [CLA.md](CLA.md).
