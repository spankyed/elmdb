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

// Integers that don't fit 32 bits, such as the times in `createdAt` and `updatedAt`, cross as 64-bit integers, which
// msgpackr decodes as BigInt unless told otherwise; 'auto' answers a number up to 2^53 and an exact BigInt past it,
// as a `number`, `any` or `object` field can hold
const msgpack = new Packr({ useRecords: false, mapsAsObjects: true, int64AsType: 'auto' });

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

db.write(msgpack.pack({ ops: [
  { op: 'create', type: 'Note', id: 'Note-a', fields: { title: 'Choosing a database' } },
  { op: 'create', type: 'Note', id: 'Note-b', fields: { title: 'Backups' } },
  { op: 'create', type: 'Note', id: 'Note-c', fields: { title: 'Database migrations' } },
  { op: 'link', source: 'Note-b', kind: 'refs', target: 'Note-a' },
  { op: 'link', source: 'Note-c', kind: 'refs', target: 'Note-a' },
] }));

const notes = { from: 'types', types: ['Note'] };
const [found, top] = msgpack.unpack(db.read(msgpack.pack({ track: 1, reads: [
  { read: 'query', plan: { start: notes, steps: [{ step: 'search', query: 'databse' }], terminal: { answer: 'ids' } } },
  { read: 'query', plan: { start: notes, steps: [{ step: 'ranked', index: 'noteRank' }, { step: 'limit', n: 1 }], terminal: { answer: 'ids' } } },
] })));

console.log(found.ids); // what the misspelled search found
console.log(top.ids);   // the most central note

const edit = { op: 'set', id: 'Note-c', fields: { title: 'Schema migrations' } };
const { changes } = msgpack.unpack(db.write(msgpack.pack({ ops: [edit] })));
console.log(changes.touched); // the tracked reads this commit wrote into: re-run token 1
db.close();
```

```text
[ 'Note-c', 'Note-a' ]
[ 'Note-a' ]
[ 1 ]
```

Reads and writes are plain data, sent as msgpack: a read is a plan, and a write is a batch. Their types are generated from the Rust: `crates/elmdb-node/protocol/`. `elmdb/client.js` takes and answers them as those types over your own msgpack codec, decoding exactly an answer whose stored maps may hold a key `__proto__`:

```js
import { Database } from 'elmdb';
import { Client } from 'elmdb/client.js';
import { Packr } from 'msgpackr';

const objects = new Packr({ useRecords: false, mapsAsObjects: true, int64AsType: 'auto' });
const maps = new Packr({ useRecords: false, mapsAsObjects: false, int64AsType: 'auto' });
const client = new Client(Database.open('./notes'), {
  pack: (value) => objects.pack(value),
  unpack: (bytes) => objects.unpack(bytes),
  unpackMaps: (bytes) => maps.unpack(bytes),
});

const [count] = client.read({ reads: [{ read: 'query', plan: { start: { from: 'types', types: ['Note'] }, terminal: { answer: 'count' } } }] });
console.log(count.n); // 3
client.db.close();
```

## Through EARS

EARS, the entity-attribute-relation system, is the fluent TypeScript API over a database: query and write builders
that compile to those plans and batches, watches, and typed facades over your own entity shapes. It is
[`@elmdb/ears`](packages/ears), in this repository, and [apack](https://github.com/spankyed/apack), elmdb's first
user, runs on it.

```js
import { createEarsEngine } from '@elmdb/ears';
import { openDatabase } from '@elmdb/ears/database';

const database = openDatabase({
  path: './notes',
  schema: {
    types: { Note: { required: ['title'], search: { fields: { title: 1 } } } },
    relations: { refs: {} },
    roles: [],
  },
});
const ears = createEarsEngine({ database });

const choosing = ears.create('Note', { title: 'Choosing a database' }).id();
ears.create('Note', { title: 'Backups' }).link('refs', choosing);
ears.create('Note', { title: 'Database migrations' }).link('refs', choosing);

const titles = (notes) => notes.map((note) => note.title);
console.log(titles(ears.query('Note').search('databse').pick(['title']))); // misspelled, most relevant first
console.log(titles(ears.query.ids([choosing]).in('refs', 'Note').pick(['title']))); // the notes that refer to it

database.close();
```

```text
[ 'Database migrations', 'Choosing a database' ]
[ 'Backups', 'Database migrations' ]
```

Each write outside `transact` is its own commit, and a read answers what is committed. What EARS holds you to, and the
rest of its API, is in [ears.md](docs/architecture/ears.md).

## Status

Running under apack, and not yet published to npm, so build it from source. Both packages are at 0.1.0, which a
release packs as `elmdb-0.1.0.tgz`, a platform package `elmdb-binding-<platform>-0.1.0.tgz` holding each of the seven
targets' binaries, of which npm installs the machine's alone, and `elmdb-ears-0.1.0.tgz`, and smoke-tests on each
target, but no release is tagged. The storage format stays at 1, edited in place,
until a release has users, so nothing here is a stable API yet.

```sh
git clone https://github.com/spankyed/elmdb.git && cd elmdb
npm ci && npm run build
npm install /path/to/elmdb/crates/elmdb-node msgpackr   # from your app, for the binding
npm install /path/to/elmdb/packages/ears                 # and for EARS beside it
```

It uses N-API, so a Node or Electron upgrade needs no rebuild.

| | |
|---|---|
| Node | 22.17 or later, and Electron through N-API |
| Platforms | macOS, Linux (glibc and musl) and Windows on x64; macOS and Linux on arm64 |
| Not in scope | a server, replication, or access control |

**Durability:** a commit is visible at once and synced within about a second. `db.write(batch, true)` returns once
its sync is done, and `await db.durable()` waits for every commit before it to be on disk without holding the event
loop. `NOT_DURABLE` means the commit landed but its sync failed, so don't blindly retry it. `SYNC_BEHIND` means nothing
was written, as the syncs that let the file be reused are failing for now: try it again after a pause.

**Writes block; long maintenance doesn't have to.** A write runs on the calling thread and answers there, so what it
created and what it refused are known at the call, and a read after it sees it. `db.checkAsync`, `db.rebuildAsync`,
`db.backupAsync` and `db.readAsync` run on a thread of the pool and answer a promise, so a large rebuild doesn't hold
the event loop; each one's plain name is its blocking form (`durable()` is a wait, and has none). A rebuild holds the
database's write lock until it commits, so a write, a unit or a schema change made while one runs waits for it, holding
the event loop: await a `rebuildAsync` before writing. A close waits for an operation that is running, so one made during a long rebuild
holds the event loop until it ends. A `readAsync` answers what `read` would have answered at its call: a write made
after the call is never in its answer.

**Power loss:** by default commits are synced in groups: once writes have paused for 100 ms, and once a second while
they go on. If the machine loses power between two syncs, the commits since the last sync are lost and nothing
before them is: the database opens at its last synced commit, and every open reports it (`db.openReport.restored`)
until `db.acknowledgeRestore()`. A test replays what a loss can leave on the file, and each night the engine runs over
a filesystem that drops and tears what wasn't synced; no machine's power has been cut under a test yet
(`docs/roadmap/synced-commit.md`, phase 5). A durable write (`db.write(batch, true)`) waits for its sync. Open with
`syncEachCommit: true` to sync every commit as it is made, its pages before the page that names them, so a commit that
returned survives a power loss; each commit then costs its own sync. A database is written in one mode at a time: a
writable open in another mode (the default, `syncEachCommit`, or `syncs: false`) than a process that has it open for
writing is refused (`MODE_MISMATCH`).

**Embeddings:** elmdb doesn't call a model. It tracks what needs embedding, your worker submits vectors, and a stale one is
refused.

## Docs

See [docs map](docs/README.md). The ones most people want:

- [background.md](docs/background.md): why elmdb exists
- [design.md](docs/architecture/design.md): principles, vocabulary, and what elmdb won't do
- [schema.md](docs/architecture/schema.md): types, fields, relations, indexes, and how a schema changes
- [engine.md](docs/architecture/engine.md): every read, write and admin operation, and the protocol
- [ears.md](docs/architecture/ears.md): EARS, the fluent TypeScript API over a database
- [roadmap.md](docs/roadmap.md): what's built, and what's waiting on what

## Develop

Needs Rust, which `rust-toolchain.toml` pins, Node, and [cargo-nextest](https://nexte.st) for the Rust suites.
`CLAUDE.md` has the full list of checks and the repository's rules.

```sh
npm ci && npm run build && npm test  # the binding and EARS, and their tests
cargo nextest run --workspace        # the Rust suites
cargo run -p elmdb-cli -- check DIR  # also: inspect, rebuild, backup, dump, load, acknowledge-restore
```

## License

Dual-licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE).

A binary of elmdb also holds the code it is built from, each under its own license: LMDB, under
[the OpenLDAP Public License](LICENSE-OPENLDAP), zstd, under BSD-3-Clause, the Rust standard library and the crates
elmdb depends on. Redistributing one means complying with those licenses too. The npm package carries their texts and
notices beside its `.node` files: the crates' in `THIRD-PARTY-NOTICES`, the Rust standard library's in
`COPYRIGHT-rust-library.html`, and LMDB's alone in `LICENSE-OPENLDAP`. The `elmdb` command owes the same notices for
what it is built from.

Unless you explicitly state otherwise, any contribution you intentionally submit for inclusion in this work, as defined in
the Apache-2.0 license, is dual-licensed as above, without any additional terms or conditions.
