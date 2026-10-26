# Schema

What a database holds and the rules it keeps, declared as data. Built: `crates/elmdb/src/schema/`.

## Principles

1. **Declare what the engine acts on, and leave the rest opaque.** A field the engine indexes, sorts, checks or reads
   text from has a type; a field it only stores can be `any`. The engine validates a value's top level, never inside an
   `object` or `any`.
2. **One canonical form.** The schema elmdb takes is verbose JSON with no shorthand and no defaults left implicit to
   the reader. Sugar belongs to whatever writes it (apack's manifest, a builder), never to the format.
3. **Rules live in the schema, not in each call.** Whether a relation may form a cycle, how many edges a pair may hold,
   and which fields a row must have are facts about the data. A caller can't forget a rule it never has to pass.
4. **A schema change never destroys primary data.** Removing a declaration stops writes to it and keeps what is
   stored; deleting it is an explicit admin operation. Derived data (indexes) is rebuilt or dropped freely.
5. **Nothing is inferred that could be wrong.** A rename isn't guessed from a removal and an addition; it is an
   operation a migration runs.
6. **Invalid is loud and complete.** An unknown key, a bad name or a dangling reference refuses the whole schema, with
   every problem listed by its path, not just the first.

## A complete example

```json
{
  "$schema": "https://elmdb.dev/schema/1.json",
  "collation": "root",
  "roles": {
    "assistant_birth": {}
  },
  "relations": {
    "contains":   { "cardinality": "one-to-many" },
    "parent_of":  { "cardinality": "one-to-many", "acyclic": true },
    "references": { "cardinality": "many-to-many", "unique": true,
                    "endpoints": [{ "source": "Note", "target": "Note" }] }
  },
  "types": {
    "Thread": {
      "declaredBy": "default-setup",
      "shortCode": { "prefix": "THR" },
      "fields": {
        "topic":                { "type": "string" },
        "pinned":               { "type": "boolean", "optional": true },
        "lastMessageTimestamp": { "type": "number", "optional": true },
        "tags":                 { "type": "list", "of": { "type": "string" }, "optional": true },
        "scratchpad":           { "type": "object", "optional": true }
      },
      "indexes": {
        "pinned": { "kind": "value", "fields": ["pinned"] }
      }
    },
    "Message": {
      "declaredBy": "default-setup",
      "fields": {
        "text":      { "type": "string" },
        "sender":    { "type": "enum", "values": ["user", "assistant", "system", "marker"] },
        "timestamp": { "type": "number" },
        "status":    { "type": "enum", "values": ["queued", "cancelled"], "optional": true, "nullable": true },
        "blocks":    { "type": "any", "optional": true }
      },
      "indexes": {
        "senderByTime": { "kind": "value", "fields": ["sender", "timestamp"] },
        "text":         { "kind": "text", "fields": { "text": { "weight": 1 } } },
        "meaning":      { "kind": "vector", "from": ["text"], "chunk": "paragraph",
                          "dims": 384, "metric": "cosine", "precision": "f32", "model": "bge-small-en-v1.5" }
      }
    },
    "IndexedDoc": {
      "declaredBy": "default-setup",
      "fields": {
        "indexId":  { "type": "ref", "to": ["SearchIndex"] },
        "chunkKey": { "type": "string" }
      },
      "indexes": {
        "chunkKey": { "kind": "value", "fields": ["chunkKey"], "unique": true }
      }
    }
  }
}
```

## The document

| Key | Holds |
|---|---|
| `$schema` | the URL of the published JSON Schema for this version of the schema language, which is also how an editor validates and completes it. A version elmdb doesn't know is refused |
| `collation` | how strings sort: `root` (ICU's root collation) or a locale (`en`, `de`). One per database |
| `types` | entity types by name |
| `relations` | relation kinds by name |
| `roles` | role names, each an object kept for future options |

Every object refuses keys it doesn't know, so a typo is an error rather than a rule silently not applied.

## Names

| Declares | Pattern | Examples |
|---|---|---|
| type | PascalCase, `^[A-Z][A-Za-z0-9]*$` | `Thread`, `BrowserTab` |
| field, index | lowerCamelCase, `^[a-z][A-Za-z0-9]*$` | `lastMessageTimestamp`, `senderByTime` |
| relation kind, role | snake_case, `^[a-z][a-z0-9_]*$` | `parent_of`, `assistant_birth` |
| short code prefix | `^[A-Z][A-Z0-9]{0,7}$`, unique across types | `THR` |

The engine's own names can't be declared: the fields `id`, `entityType`, `shortCode`, `createdAt`, `updatedAt`,
`trashedAt` and `role`, and the types `Relation` and `Sequence`, which the engine's own ids are written with.

## Types

```json
"Message": {
  "declaredBy": "default-setup",
  "shortCode": { "prefix": "MSG" },
  "fields": { … },
  "additionalFields": "refuse",
  "indexes": { … }
}
```

- `declaredBy` is an opaque string the engine puts in every error about the type, so an error names who declared what
  it refused. apack sets it to the pack's id. Relation kinds and roles take one too.
- `shortCode`: the engine numbers each created row `<prefix>-<n>` and never hands a number out twice.
- `additionalFields`: `refuse` (the default) refuses a write to a field the type doesn't declare; `allow` stores it as
  `any`.

## Fields

| `type` | Holds | Options |
|---|---|---|
| `string` | text | |
| `number` | a finite double | |
| `integer` | a whole number within ±2^53 | |
| `boolean` | `true` or `false` | |
| `bytes` | binary | |
| `enum` | one of the listed strings | `values` |
| `ref` | an entity's id, held as a value rather than as a relation | `to`: the types it may name |
| `list` | an array | `of`: the element's field type, without `optional` and `nullable` |
| `object` | a map, opaque inside | |
| `any` | anything msgpack holds | |

Every field also takes:

- `optional: true`: the row may lack it. A field that isn't optional is required: `create` refuses a row without it and
  `delete` refuses to remove it.
- `nullable: true`: it may hold `null`.

These are TypeScript's `?` and `| null`, kept apart because they mean different things.

A `ref` lets `check()` report ids that name no entity and lets a plan follow one like an edge, without the field being
a relation. A relation is the better choice when the link has a kind, info or an order; a `ref` suits a foreign key
such as a chunk's `indexId`.

## Relations

```json
"parent_of": {
  "cardinality": "one-to-many",
  "acyclic": true,
  "unique": true,
  "endpoints": [{ "source": "Collection", "target": "Collection" }, { "source": "Collection", "target": "Document" }],
  "info": { "type": "object", "optional": true }
}
```

- `cardinality`, from the source's side: `one-to-one`, `one-to-many` (a target has at most one source: a child has one
  parent), `many-to-one` (a source has at most one target), `many-to-many`. It counts far ends, not edges: a second
  edge between the same two ends is no second source. A link past it is refused (`CARDINALITY`);
  moving a child is an unlink and a link in one batch.
- `acyclic: true`: a link that would close a cycle through this kind is refused (`CYCLE`). Today's per-call
  `acyclicIn` becomes this.
- `unique: true`: at most one edge of this kind between two entities, so linking a pair again updates the edge's info,
  and moving an edge onto a pair another edge joins is refused (`DUPLICATE_EDGE`).
  Today's per-call `replace` becomes this.
- `endpoints`: the type pairs an edge may join. Absent, any.
- `info`: the field type of the edge's info. Absent, `any`.

An entity's edges keep the order they were linked in.

## Indexes

Indexes are declared on the type, named, and separate from the fields, since one index can cover several fields and one
field several indexes. A plan never names a value index: the engine picks one from the plan's filters and order. A
search step names its text or vector index only when the type has more than one.

**Value**: equality, ranges and order.

```json
"senderByTime": { "kind": "value", "fields": ["sender", "timestamp"], "unique": false }
```

- `fields`: one, or several for a compound index, which serves a filter on its leading fields together with order or a
  range on the next: `where('sender', 'user').orderBy('timestamp')`. A field of any type but `bytes` and `object`: of
  an `any` field the index holds the scalar values, and a lookup for another walks the type's rows.
- `unique: true` refuses a write that would give two entities the same values (`UNIQUE`).
- A `list` field is indexed per element. A string longer than 256 bytes (UTF-8) isn't indexed: the type's rows are walked
  for it instead.
- Every type has a unique value index on `shortCode` and one on `role`, without declaring them.

**Text**: ranked full-text search (`search.md`).

```json
"text": { "kind": "text", "fields": { "title": { "weight": 3 }, "body": { "weight": 1 } }, "analyzer": "standard" }
```

- `fields`: `string` fields, lists of strings, or `any` fields, each with a weight; an `any` field's value is text
  when it is a string or a list of strings, and no text otherwise.
- `analyzer`: `standard` (charabia's tokenizing, identifiers split on case and punctuation, case and accents folded) or
  `exact` (whole values, case folded). Stemming is `standard`'s option `{ "analyzer": "standard", "stem": "en" }`.

**Vector**: nearest neighbours (`search.md`).

```json
"meaning": { "kind": "vector", "from": ["title", "body"], "chunk": "paragraph",
             "dims": 384, "metric": "cosine", "precision": "f32", "model": "bge-small-en-v1.5" }
```

- `from`: the `string` fields the text to embed is made of. `chunk`: `whole`, `paragraph` or `heading` (Markdown
  sections).
- `dims`, `metric` (`cosine` or `l2`), `precision` (`f32`). hannoy's graphs measure cosine and euclidean distance over
  f32 vectors; a normalized embedding ranks by dot product as it does by cosine.
- `model`: an opaque name the engine records with each vector. A changed model makes every vector pending again, and
  a vector written under another model is refused.

## Changing a schema

A database stores its schema (`m/schema`) and its canonical hash. Opening with a schema compares the two, applies the
change in one unit, and returns a report of what it did; reading without a schema (`readOnly`, or a tool) uses the
stored one.

| Change | What happens |
|---|---|
| A type, field, relation kind, role or index added | applied. A new index builds in the background; plans walk the rows until it's built |
| An index's definition changed | rebuilt in the background under its name |
| The collation changed | applied: an index holds its values as written, and the collation orders `orderBy` as a query runs |
| An index removed | dropped |
| A field, type, relation kind or role removed | the declaration goes and the data stays: writes to it are refused, `admin` reads it, and `admin.purge` deletes it |
| A rule tightened (a field made required, a type narrowed, a cardinality, `acyclic`, `unique` added) | enforced on every write from then on. `check()` reports existing rows that break it. A `unique` index whose rows hold duplicates isn't built, and the report names them |
| A short code prefix changed | refused: a code is an entity's identity |
| Anything renamed | not inferred. `admin.renameType`, `renameField` and `renameRelation` rename stored data in one unit, run from a migration |

Rows that predate a tightened rule are reported, not rewritten: fixing them is a migration's job, which knows what the
right value is.

The hash is of the canonical form (keys sorted, every default written out), so two schemas that mean the same thing
have the same hash and an unchanged schema costs one comparison at open.

## Errors

`SCHEMA_INVALID` lists every problem with its path and, where `declaredBy` is set, who declared it. Problems of form
(an unknown key, a missing or mistyped value) are all listed first; the names, references and index rules need the whole
document to read, so they're checked, and every one of their problems listed, once nothing of form is wrong:

```
types.Message.indexes.text.fields.body: no such field on Message (declared by default-setup)
relations.parent_of.endpoints[1].target: no such type "Documnet"
```

## Generated from it

elmdb's Rust schema types are the definition. From them the build generates:

- the TypeScript types of the schema (`Schema`, `protocol/Schema.ts`), published in the `elmdb` package;
- the JSON Schema at `$schema`'s URL, for editors and for validating a schema before opening a database.

## apack's schema

apack doesn't write this document. Codegen writes a fragment per pack from its manifest and shapes, and the host merges
the registered packs' fragments, refusing a name two packs declare and naming both, as it does today:

- **types, short code prefixes and indexes** from each `apack.json` entity entry. Its `indexes: ["pinned"]` stays the
  manifest's shorthand for a value index per field, and gains the text and vector forms;
- **fields** from the TypeScript shape: `string`, `number` and `boolean` as themselves, a union of string literals as
  `enum`, `T[]` as `list`, a branded `EARS.EntityId<'X'>` as `ref` to `X`, an interface or record as `object`, any
  other union as `any`; `?` as `optional` and `| null` as `nullable`;
- **relation kinds and roles** from `data.relations` and `data.roles`, which gain the relation options;
- **`declaredBy`** as the pack's id.

`additionalFields` is `refuse` for every apack type, since every field a pack writes is in its shape.
