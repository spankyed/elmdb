<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="docs/assets/banner-dark.svg">
    <img src="docs/assets/banner-light.svg" width="600" alt="elmdb">
  </picture>
  <br>
  <em>Say "LMDB" out loud: that's the name.</em>
</p>

An embedded database for Node.js and Electron, with search, graphs and live queries built in. It runs inside your app,
with no server, so you don't need a separate search index or your own cache invalidation. Written in Rust on LMDB.

- **Search:** full text and vectors, plus fuzzy, substring and regex matching.
- **Graphs:** paths, communities and PageRank, over the relations between your data.
- **Live queries:** every commit says which reads it affected, so a view re-runs only when it needs to.
- **Enforced schemas:** you declare your types and relations, and every write is checked against them.
- **Repairable indexes:** `check()` verifies them against your data, and `rebuild()` replaces them.

A good fit for knowledge tools, document libraries and other desktop apps with connected data.

## Example

```js
import { Database } from 'elmdb';
import { Packr } from 'msgpackr';

const msgpack = new Packr({ useRecords: false, mapsAsObjects: true });

const db = Database.open('./notes', {
  schema: {
    $schema: 'https://elmdb.dev/schema/1.json',
    types: {
      Note: {
        fields: { title: { type: 'string' } },
        indexes: { text: { kind: 'text', fields: { title: {} } } },
      },
    },
    relations: { refs: { cardinality: 'many-to-many' } },
    ranks: { noteRank: { types: ['Note'], relations: ['refs'] } },
  },
});

db.writeSync(msgpack.pack({ ops: [
  { op: 'create', type: 'Note', id: 'Note-a', fields: { title: 'Choosing a database' } },
  { op: 'create', type: 'Note', id: 'Note-b', fields: { title: 'Backups' } },
  { op: 'create', type: 'Note', id: 'Note-c', fields: { title: 'Database migrations' } },
  { op: 'link', source: 'Note-b', kind: 'refs', target: 'Note-a' },
  { op: 'link', source: 'Note-c', kind: 'refs', target: 'Note-a' },
] }));

const notes = { from: 'types', types: ['Note'] };
const [found, top] = msgpack.unpack(db.readSync(msgpack.pack({ track: 1, reads: [
  { read: 'query', plan: { start: notes, steps: [{ step: 'search', query: 'databse' }], terminal: { answer: 'ids' } } },
  { read: 'query', plan: { start: notes, steps: [{ step: 'ranked', index: 'noteRank' }, { step: 'limit', n: 1 }], terminal: { answer: 'ids' } } },
] })));

console.log(found.ids); // what the misspelled search found
console.log(top.ids);   // the most central note

const edit = { op: 'set', id: 'Note-c', fields: { title: 'Schema migrations' } };
const { changes } = msgpack.unpack(db.writeSync(msgpack.pack({ ops: [edit] })));
console.log(changes.touched); // the tracked reads this commit wrote into: re-run token 1
db.close();
```

```text
[ 'Note-c', 'Note-a' ]
[ 'Note-a' ]
[ 1 ]
```

Reads and writes are plain data, sent as msgpack: a read is a plan, and a write is a batch. Their types are generated from the Rust: `crates/elmdb-node/protocol/`. [apack](https://github.com/spankyed/apack),
elmdb's first user, puts a typed query builder over it in `@apackdev/ears`.

## Status

Running under apack, and not yet published, so build it from source. The storage format stays at 1, edited in place,
until a release has users, so nothing here is a stable API yet.

```sh
git clone https://github.com/spankyed/elmdb.git && cd elmdb
npm ci && npm run build
npm install /path/to/elmdb/crates/elmdb-node msgpackr   # from your app
```

It uses N-API, so a Node or Electron upgrade needs no rebuild.

| | |
|---|---|
| Node | 22.17 or later, and Electron through N-API |
| Platforms | macOS, Linux (glibc and musl) and Windows on x64; macOS and Linux on arm64 |
| Not in scope | a server, replication, or access control |

**Durability:** a commit is visible at once and synced within about a second. `db.write(batch, true)` resolves once its
sync is done, and `db.writeSync(batch, true)` returns then. `NOT_DURABLE` means the commit landed but its sync failed, so
don't blindly retry it.

**Off the calling thread:** `db.write`, `db.check`, `db.rebuild` and `db.backup` run on a thread of the pool and answer a
promise, so a large rebuild doesn't hold the event loop; `db.writeSync`, `db.checkSync`, `db.rebuildSync` and
`db.backupSync` are the blocking forms. Writes and rebuilds are applied one at a time in the order you asked for them,
so a write sees what the one asked before it committed, and however many are waiting on a database they hold one of
Node's pool threads between them. Await a write whose answer or failure the next one depends on, and before a blocking call that
must come after it: `writeSync`, `begin` and the other blocking calls don't wait their turn among them. A close waits
for an operation that is running, so one made during a long rebuild holds the event loop until it ends.

**Power loss:** by default commits are synced in groups: once writes have paused for 100 ms, and once a second while
they go on. If the machine loses power between two syncs, the commits since the last sync can be lost. The file can
also be left holding part of a commit, since nothing orders what is written between syncs: it may then be refused when
it is opened (`CORRUPTED`), read wrongly, or end the process that reads it, and every earlier commit is lost with it.
The remedy is a backup. A write's `durable: true` waits for its sync; it doesn't keep a later commit from leaving the
file that way. Open with `syncEachCommit: true` to sync every commit as it is made, its pages before the page that
names them, so a commit that returned survives a power loss and the file stays whole; each commit then costs its own
sync.

**Embeddings:** elmdb doesn't call a model. It tracks what needs embedding, your worker submits vectors, and a stale one is
refused.

## Docs

See [docs map](docs/README.md). The ones most people want:

- [background.md](docs/background.md): why elmdb exists
- [design.md](docs/architecture/design.md): principles, vocabulary, and what elmdb won't do
- [schema.md](docs/architecture/schema.md): types, fields, relations, indexes, and how a schema changes
- [engine.md](docs/architecture/engine.md): every read, write and admin operation, and the protocol
- [roadmap.md](docs/roadmap.md): what's built, and what's waiting on what

## Develop

Needs Rust, which `rust-toolchain.toml` pins, and Node. `CLAUDE.md` has the full list of checks and the repository's rules.

```sh
npm ci && npm run build && npm test  # the binding and its tests
cargo nextest run --workspace        # the Rust suites
cargo run -p elmdb-cli -- check DIR  # also: inspect, backup, dump, load
```

## License

Dual-licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE).

Unless you explicitly state otherwise, any contribution you intentionally submit for inclusion in this work, as defined in
the Apache-2.0 license, is dual-licensed as above, without any additional terms or conditions.
