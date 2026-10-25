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

## Open questions

- Where the declaration lives in a pack: beside the manifest, or as the manifest's `data` section.
- Editor completions, which apack's typed EARS contract is held to (`TYPED-EARS.md` in apack), from generated types.
- How a field a pack stores without declaring (an `additionalFields: allow` type) is typed.
