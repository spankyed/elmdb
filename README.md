# elmdb

An embedded graph database for ECMAScript, written in Rust on LMDB.

The name is E (ECMAScript) and LMDB, and it's how "LMDB" sounds said out loud.

elmdb holds typed entities, their attributes and the relations between them. It keeps value, text and vector indexes
in the same transaction as the data, runs a read as one plan and a write as one batch, and reports what every commit
changed, so a view can stay current. It knows no application: types, relation kinds and rules arrive as a schema. Its
first user is apack, whose EARS engine it replaces underneath an unchanged API.

## Docs

- [design.md](docs/design.md): principles, packages, vocabulary, conventions, the boundary with apack
- [schema.md](docs/schema.md): types, fields, relations, indexes, and how a schema changes
- [engine.md](docs/engine.md): the operations and the protocol
- [search.md](docs/search.md): text and vector indexes, and search as plan steps
- [format.md](docs/format.md): the key layout, values, versions and LMDB
- [testing.md](docs/testing.md): what proves it correct
- [roadmap.md](docs/roadmap.md): the order of work
- [background.md](docs/background.md): why elmdb exists

## Status

Not started. The first step is a spike measuring what the protocol costs against apack's engine today.
