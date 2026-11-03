<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="docs/assets/banner-dark.svg">
    <img src="docs/assets/banner-light.svg" width="600" alt="elmdb">
  </picture>
  <br>
  <em>Say "LMDB" out loud: that's the name.</em>
</p>

An embedded database for Node.js and Electron, written in Rust on LMDB. It holds typed entities and the relations between
them, keeps its value and text indexes in the same transaction as the data, and tells you what every commit changed.

- **Typed data:** your schema declares types, fields and relation kinds. Every writer is held to it.
- **Search:** ranked full text, fuzzy terms, substrings and regex, and vector and hybrid search.
- **Graphs:** walks and shortest paths, components, communities and PageRank, computed in a read, or kept current as a rank index.
- **Live queries:** each commit names the tracked reads it wrote into, so a view re-runs only when its answer could change.
- **Repairable:** every index can be rebuilt from primary data, and `check()` compares them.

Reads and writes are data: a read is a plan and a write is a batch, each crossing into Rust once as msgpack.

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

db.write(msgpack.pack({ ops: [
  { op: 'create', type: 'Note', id: 'Note-a', fields: { title: 'Choosing a database' } },
  { op: 'create', type: 'Note', id: 'Note-b', fields: { title: 'Backups' } },
  { op: 'create', type: 'Note', id: 'Note-c', fields: { title: 'Database migrations' } },
  { op: 'link', source: 'Note-b', kind: 'refs', target: 'Note-a' },
  { op: 'link', source: 'Note-c', kind: 'refs', target: 'Note-a' },
] }));

const notes = { from: 'types', types: ['Note'] };
const [found, top] = msgpack.unpack(db.readSync(msgpack.pack({ reads: [
  { read: 'query', plan: { start: notes, steps: [{ step: 'search', query: 'databse' }], terminal: { answer: 'ids' } } },
  { read: 'query', plan: { start: notes, steps: [{ step: 'ranked', index: 'noteRank' }, { step: 'limit', n: 1 }], terminal: { answer: 'ids' } } },
] })));

console.log(found.ids); // what the misspelled search found
console.log(top.ids);   // the most central note
db.close();
```

```text
[ 'Note-c', 'Note-a' ]
[ 'Note-a' ]
```

The protocol's types are generated from the Rust: `crates/elmdb-node/protocol/`. [apack](https://github.com/spankyed/apack),
elmdb's first user, puts a typed query builder over it in `@apackdev/ears`.

## Status

Running under apack, and not yet published: build it from source. The storage format stays at 1, edited in place,
until a release has users, so nothing here is a stable API yet.

| | |
|---|---|
| Node | 22.17 or later, and Electron through N-API |
| Platforms | macOS, Linux (glibc and musl) and Windows on x64; macOS and Linux on arm64 |
| Not in scope | a server, replication, access control, or calling embedding models |

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
