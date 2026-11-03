# Docs

What each directory is for, so you know how far to trust what you're reading.

| Directory | Holds | Read it as |
|---|---|---|
| [`architecture/`](architecture/design.md) | how elmdb works today | current: a change to the code updates it |
| [`roadmap.md`](roadmap.md), [`roadmap/`](roadmap/README.md) | what is built, and designs waiting on a trigger | intent: each file says how far it is built |
| [`evidence/`](evidence/) | measurements behind decisions | dated: true of the commit and machine it names |
| [`archive/`](archive/) | closed goals, and designs since built | history: the code and `architecture/` win over it |
| [`reviews/`](reviews/) | point-in-time assessments and proposals | dated: findings as of the review |

## Architecture

- [design.md](architecture/design.md): principles, packages, vocabulary, and what's out of scope. Start here.
- [schema.md](architecture/schema.md): types, fields, relations, indexes, rank indexes, and how a schema changes.
- [engine.md](architecture/engine.md): opening, plans, batches and units, durability, watch, admin, errors.
- [search.md](architecture/search.md): text, trigram and vector indexes, and search as plan steps.
- [format.md](architecture/format.md): the key layout, values, versions, dumps and LMDB.
- [filesystems.md](architecture/filesystems.md): where a database may live.
- [testing.md](architecture/testing.md): what proves it correct, and the commands for each check.

[background.md](background.md) is why elmdb exists.
