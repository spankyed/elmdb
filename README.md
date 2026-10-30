<h1 align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="docs/assets/banner-dark.svg">
    <img src="docs/assets/banner-light.svg" width="600" alt="elmdb">
  </picture>
</h1>

Say "LMDB" out loud: that's the name.

Embedded database for Node.js and Electron: typed entities and relations, text and vector search, and live queries, all in one transaction. Rust on LMDB.

elmdb holds typed entities, their attributes and the relations between them. It keeps value, text and vector indexes
in the same transaction as the data, runs a read as one plan and a write as one batch, and reports what every commit
changed, so a view can stay current. It assumes nothing about your application: your schema defines the types,
relation kinds and rules.

## Docs

- [design.md](docs/architecture/design.md): principles, packages, vocabulary, conventions, the boundary with apack
- [schema.md](docs/architecture/schema.md): types, fields, relations, indexes, and how a schema changes
- [engine.md](docs/architecture/engine.md): the operations and the protocol
- [search.md](docs/architecture/search.md): text and vector indexes, and search as plan steps
- [format.md](docs/architecture/format.md): the key layout, values, versions and LMDB
- [testing.md](docs/architecture/testing.md): what proves it correct
- [roadmap.md](docs/roadmap.md): the order of work
- [background.md](docs/background.md): why elmdb exists

## Status

Built and running apack: the schema, the engine and its protocol, durability, value, text and vector indexes, change
sets with tracked reads, the admin operations and the CLI, with the model, crash and fuzz suites
(`docs/architecture/testing.md`). The order of work, and what comes next, is `docs/roadmap.md`.

```sh
cargo nextest run --workspace   # the Rust suites
cargo test --workspace --doc    # and their doc tests
npm run build && npm test  # the Node binding, and its tests
```
