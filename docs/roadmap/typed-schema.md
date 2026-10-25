# The schema as the source of types

Status: idea.

## Where it starts

apack declares a row's shape in TypeScript, and codegen reads it to build the schema it sends elmdb: required fields as
`any`, indexed fields as values, every other field allowed. The schema elmdb validates is thinner than the one apack
wrote, and the two have already disagreed: entity names elmdb can't hold (D25), and text indexes that had to accept `any`
fields because a shape's types never reach the schema (D26).

## The idea

The schema is written once, as elmdb's schema (or a TypeScript DSL that compiles to it), and the TypeScript shapes, the
typed facades' types and the manifest's entries are generated from it. A field's type is checked by the engine on every
write and by the compiler on every call site, from one declaration; enums, refs, lists and cardinalities that the schema
already models become types apack's code sees.

## The other way: a schema as thin as its user

The decision has two sides, and this direction is one. The other is that elmdb's schema keeps only what apack uses —
types, required fields, indexes, relation kinds with their rules, roles and short codes — and drops field types,
enums, refs, endpoints and cardinalities until a user asks for them. It costs less to keep, and nothing apack sends
today would notice.

What decides between them is whether a field's type is worth checking twice, by the compiler and by the engine. The
engine's check catches a write from a script, a migration or a pack without generated types; the compiler's catches
everything else, earlier. Generating types from the schema is the case for keeping the schema rich; a thin schema is
the case that the compiler's check is enough.

## Open questions

- Where the declaration lives in a pack: beside the manifest, or as the manifest's `data` section.
- Editor completions, which apack's typed EARS contract is held to (`TYPED-EARS.md` in apack), from generated types.
- How a field a pack stores without declaring (an `additionalFields: allow` type) is typed.
