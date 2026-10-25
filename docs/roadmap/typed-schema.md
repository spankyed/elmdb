# Field types from apack's shapes

Status: decided, 2026-10-25; planned in apack (`docs/plans/field-types-from-shapes.md`).

## Where it starts

apack declares a row's shape in TypeScript, and codegen reads it to build the schema it sends elmdb: required fields as
`any`, indexed fields as values, every other field allowed. The schema elmdb validates is thinner than the one apack
wrote, and the two have already disagreed: entity names elmdb can't hold (D25), and text indexes that had to accept `any`
fields because a shape's types never reach the schema (D26). elmdb checks a field's value on every write, from every
writer, but only for a field the schema types, and apack types none.

## The decision

**apack's shapes stay the one source, and codegen sends each field's type in the schema.** Codegen already reads each
shape with the TypeScript checker; it maps `string`, `number`, `boolean`, a union of string literals, an id tagged with
its types, a list of one of these, nullability and optionality to elmdb's field types, and anything else to `any`. The
engine's checks then pay off on the writes the compiler can't see: a console session, a migration, a script, an untyped
write. Nothing in elmdb changes; apack starts using what is there.

## The directions set aside

- **The schema generating the shapes.** One declaration in elmdb's schema, the TypeScript shapes generated from it. It
  makes two sources while the move happens, gives up what a shape can say in TypeScript, and buys nothing the direction
  above doesn't: apack's shapes already generate every other derivation through codegen.
- **A schema as thin as its user.** Drop field types, enums, refs, endpoints and cardinalities until a user asks for
  them. It costs less to keep, and nothing apack sends today would notice — but it removes the checks the decision puts
  to use, and makes elmdb apack-shaped rather than a library a second user could take.

## What is left open

Once apack sends field types, what it still doesn't use — relation endpoints, cardinalities other than many-to-many,
compound value indexes — is decided on its own: kept for a second user, or removed.
