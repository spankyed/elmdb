// The query builder's types, read by the compiler: these assertions fail `tsc` (the package's typecheck), not vitest,
// which only sees that each case was declared. Over `_support/typed-schema.ts`'s two types.
import { describe, expectTypeOf, it } from 'vitest';
import type { QueryBuilder, UntypedSchema } from '../../src/index.ts';
import { query, typeOnly, type EARS, type Schema } from '../_support/typed-schema.ts';

type SchemaQuery<E extends string> = QueryBuilder<E, Schema>;
/** What each of the members `M` answers on a builder of `E` */
type Returns<E extends string, M extends keyof SchemaQuery<E>> = { [K in M]: ReturnType<SchemaQuery<E>[K]> };

declare const noteId: EARS.EntityId<'Note'>;
declare const plainId: EARS.EntityId;
// Generic members are asserted through an instantiation expression (`typeof b.ofType<'Folder'>`): a conditional type
// (`Q extends { ofType(t: T): infer R }`) instantiates the generic at its constraint, and would pass whatever it returned
declare const untypedBuilder: SchemaQuery<string>;
declare const noteBuilder: SchemaQuery<'Note'>;
declare const folderBuilder: SchemaQuery<'Folder'>;

describe('the query entry points', () => {
  it('type each by what it starts from: a declared name, a tagged id, or neither', () => typeOnly(() => {
    expectTypeOf(query('Note')).toEqualTypeOf<SchemaQuery<'Note'>>();
    expectTypeOf(query('Folder')).toEqualTypeOf<SchemaQuery<'Folder'>>();
    expectTypeOf(query.ids(noteId)).toEqualTypeOf<SchemaQuery<'Note'>>();
    expectTypeOf(query.ids([noteId])).toEqualTypeOf<SchemaQuery<'Note'>>();
    expectTypeOf(query.ids(plainId)).toEqualTypeOf<SchemaQuery<string>>();
    expectTypeOf(query.ids([plainId])).toEqualTypeOf<SchemaQuery<string>>();
    expectTypeOf(query.all()).toEqualTypeOf<SchemaQuery<string>>();
    expectTypeOf(query('Note' as string)).toEqualTypeOf<SchemaQuery<string>>();
    expectTypeOf(query(['Note', 'Folder'])).toEqualTypeOf<SchemaQuery<string>>();
    expectTypeOf(query({} as EARS.Entity)).toEqualTypeOf<SchemaQuery<EARS.Entity>>();
    // Through a caller's generic parameter, as well as a literal
    const byName = <E extends Schema['names']>(entityType: E) => query(entityType);
    expectTypeOf(byName<'Note'>).returns.toEqualTypeOf<SchemaQuery<'Note'>>();
    const byId = <E extends string>(id: EARS.EntityId<E>) => query.ids(id);
    expectTypeOf(byId<'Note'>).returns.toEqualTypeOf<SchemaQuery<'Note'>>();
  }));

  it('refuse an undeclared name, an id where a type is meant, and a type that may be absent', () => typeOnly(() => {
    // @ts-expect-error not a declared entity name
    query('Noet');
    // @ts-expect-error not a declared entity name, in a list
    query(['Note', 'Noet']);
    // @ts-expect-error an id starts from query.ids
    query(noteId);
    // @ts-expect-error an id literal is not an entity type either
    query('Note-abc123');
    // @ts-expect-error query.all() is every entity, asked for by name
    query('Note' as string | undefined);
  }));
});

it('ofType() narrows an untyped builder, or one of another type, to a declared type alone', () => {
  expectTypeOf<ReturnType<typeof untypedBuilder.ofType<'Folder'>>>().toEqualTypeOf<SchemaQuery<'Folder'>>();
  expectTypeOf<ReturnType<typeof noteBuilder.ofType<'Folder'>>>().toEqualTypeOf<SchemaQuery<'Folder'>>();
  typeOnly(() => {
    // @ts-expect-error not a declared entity name
    untypedBuilder.ofType('Fodler');
  });
});

describe('a filter', () => {
  type Filter = 'where' | 'orderBy' | 'limit' | 'distinct' | 'reverse' | 'within' | 'withRole' | 'forEach'
    | 'linkedWith' | 'linkedTo' | 'linkedFrom';

  it("keeps the builder's type", () => {
    expectTypeOf<Returns<'Note', Filter>>().toEqualTypeOf<Record<Filter, SchemaQuery<'Note'>>>();
    expectTypeOf<Returns<'Folder', Filter>>().toEqualTypeOf<Record<Filter, SchemaQuery<'Folder'>>>();
  });

  it('takes only the fields, kinds and roles the schema declares', () => typeOnly(() => {
    query('Note').where('title', 'x').has('pinned').lacks('body').orderBy('createdAt').withRole('starred').linkedTo('contains');
    // @ts-expect-error a field the Note shape doesn't have
    query('Note').where('name', 'x');
    // @ts-expect-error a relation kind the schema doesn't declare
    query('Note').linkedTo('containz');
    // @ts-expect-error a role the schema doesn't declare
    query('Note').withRole('stared');
  }));
});

// out and in walk to the entities at an edge's far end (the linked* steps filter in place)
it('a walk moves to the type it is given, and to an untyped builder given none', () => {
  expectTypeOf<ReturnType<typeof folderBuilder.out<'Note'>>>().toEqualTypeOf<SchemaQuery<'Note'>>();
  expectTypeOf<ReturnType<typeof noteBuilder.in<'Folder'>>>().toEqualTypeOf<SchemaQuery<'Folder'>>();
  typeOnly(() => expectTypeOf(query('Folder').out('contains')).toEqualTypeOf<SchemaQuery<string>>());
});

describe('the terminals', () => {
  it("answer ids tagged with the builder's type, and plain ones for an untyped builder", () => {
    expectTypeOf<Returns<'Folder', 'ids' | 'first' | 'last' | 'groupBy' | 'count' | 'exists'>>().toEqualTypeOf<{
      ids: EARS.EntityId<'Folder'>[];
      first: EARS.EntityId<'Folder'> | undefined;
      last: EARS.EntityId<'Folder'> | undefined;
      groupBy: Map<unknown, EARS.EntityId<'Folder'>[]>;
      count: number;
      exists: boolean;
    }>();
    expectTypeOf<ReturnType<SchemaQuery<'Note'>['ids']>>().toEqualTypeOf<EARS.EntityId<'Note'>[]>();
    expectTypeOf<ReturnType<SchemaQuery<string>['ids']>>().toEqualTypeOf<EARS.EntityId[]>();
  });

  it('pick rows of the fields picked, typed by the shape', () => typeOnly(() => {
    const [row] = query('Note').pick(['title', 'pinned']);
    expectTypeOf(row!.title).toEqualTypeOf<string>();
    expectTypeOf(row!.pinned).toEqualTypeOf<boolean | undefined>();
    expectTypeOf(row!.id).toEqualTypeOf<EARS.EntityId<'Note'>>();
    // @ts-expect-error a field the Folder shape doesn't have
    query('Folder').pick(['title']);
  }));
});

it('QueryBuilder without type arguments is the untyped builder, whose members answer untyped builders', () => {
  expectTypeOf<QueryBuilder>().toEqualTypeOf<QueryBuilder<string, UntypedSchema>>();
  expectTypeOf<ReturnType<QueryBuilder['where']>>().toEqualTypeOf<QueryBuilder<string>>();
});
