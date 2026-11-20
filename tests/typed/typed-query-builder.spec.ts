// The query builder's types, read by the compiler: these assertions fail `tsc` (the package's typecheck), not vitest,
// which only sees that each type-level case was declared. Over `_support/typed-schema.ts`'s two types.
import { describe, expectTypeOf, it } from 'vitest';
import type { QueryBuilder, UntypedSchema } from '../../src/index.ts';
import { query, type EARS, type Schema } from '../_support/typed-schema.ts';

/** A query builder over the schema's names: its entity shapes and names, its relation kinds and its roles */
type SchemaQuery<E extends string> = QueryBuilder<E, Schema>;

describe('the query entry points', () => {
  it("query('Note') and query('Folder') are builders of that type, query.all() and query.ids of plain ids untyped ones", () => {
    // Type-level only: never called, since no engine is bound to the facade
    const check = (plainIds: EARS.EntityId[]) => {
      expectTypeOf(query('Note')).toEqualTypeOf<SchemaQuery<'Note'>>();
      expectTypeOf(query('Folder')).toEqualTypeOf<SchemaQuery<'Folder'>>();
      expectTypeOf(query.all()).toEqualTypeOf<SchemaQuery<string>>();
      expectTypeOf(query.ids(plainIds)).toEqualTypeOf<SchemaQuery<string>>();
    };
    expectTypeOf(check).toBeFunction();
  });

  // What each entry point takes: query a type (a declared name, or one known only at runtime), query.ids ids, typed
  // by the entity an id is tagged with. An id where a type is meant, or a type that may be absent, is refused.
  it('types each entry point by what it starts from', () => {
    // Type-level only: never called
    const check = (
      noteId: EARS.EntityId<'Note'>, plainId: EARS.EntityId, noteIds: EARS.EntityId<'Note'>[], runtimeName: string,
      maybeName: string | undefined, anyEntity: EARS.Entity,
    ) => {
      expectTypeOf(query('Note')).toEqualTypeOf<SchemaQuery<'Note'>>();
      expectTypeOf(query.ids(noteId)).toEqualTypeOf<SchemaQuery<'Note'>>();
      expectTypeOf(query.ids(noteIds)).toEqualTypeOf<SchemaQuery<'Note'>>();
      expectTypeOf(query.ids(plainId)).toEqualTypeOf<SchemaQuery<string>>();
      expectTypeOf(query(runtimeName)).toEqualTypeOf<SchemaQuery<string>>();
      expectTypeOf(query(['Note', 'Folder'])).toEqualTypeOf<SchemaQuery<string>>();
      expectTypeOf(query(anyEntity)).toEqualTypeOf<SchemaQuery<EARS.Entity>>();
      const byName = <E extends Schema['names']>(entityType: E) => query(entityType);
      expectTypeOf(byName<'Note'>).returns.toEqualTypeOf<SchemaQuery<'Note'>>();
      const byId = <E extends string>(id: EARS.EntityId<E>) => query.ids(id);
      expectTypeOf(byId<'Note'>).returns.toEqualTypeOf<SchemaQuery<'Note'>>();
      // @ts-expect-error not a declared entity name
      query('Noet');
      // @ts-expect-error not a declared entity name, in a list
      query(['Note', 'Noet']);
      // @ts-expect-error an id is not an entity type: it starts from query.ids
      query(noteId);
      // @ts-expect-error an id literal is not an entity type either
      query('Note-abc123');
      // @ts-expect-error a type that may be absent: query.all() is every entity, asked for by name
      query(maybeName);
    };
    expectTypeOf(check).toBeFunction();
  });
});

// Asserted through an instantiation expression (`typeof b.ofType<'Folder'>`) rather than a conditional type:
// `Q extends { ofType(t: T): infer R }` instantiates the generic at its constraint and infers the untyped builder, so
// it would pass whatever ofType returned. Type-level only, so the declared consts need no value.
declare const untypedBuilder: SchemaQuery<string>;
declare const noteBuilder: SchemaQuery<'Note'>;
declare const folderBuilder: SchemaQuery<'Folder'>;

describe('ofType() narrows', () => {
  it('an untyped builder to one type', () => {
    expectTypeOf<ReturnType<typeof untypedBuilder.ofType<'Folder'>>>().toEqualTypeOf<SchemaQuery<'Folder'>>();
  });

  it('a builder of one type to another', () => {
    expectTypeOf<ReturnType<typeof noteBuilder.ofType<'Folder'>>>().toEqualTypeOf<SchemaQuery<'Folder'>>();
  });

  it('refuses a name the schema does not declare', () => {
    const check = (builder: SchemaQuery<string>) => {
      // @ts-expect-error not a declared entity name
      builder.ofType('Fodler');
    };
    expectTypeOf(check).toBeFunction();
  });
});

describe('a filter keeps the builder\'s type', () => {
  it('where, orderBy, limit, distinct, reverse, within, withRole and forEach', () => {
    expectTypeOf<ReturnType<SchemaQuery<'Note'>['where']>>().toEqualTypeOf<SchemaQuery<'Note'>>();
    expectTypeOf<ReturnType<SchemaQuery<'Folder'>['orderBy']>>().toEqualTypeOf<SchemaQuery<'Folder'>>();
    expectTypeOf<ReturnType<SchemaQuery<'Note'>['limit']>>().toEqualTypeOf<SchemaQuery<'Note'>>();
    expectTypeOf<ReturnType<SchemaQuery<'Folder'>['distinct']>>().toEqualTypeOf<SchemaQuery<'Folder'>>();
    expectTypeOf<ReturnType<SchemaQuery<'Note'>['reverse']>>().toEqualTypeOf<SchemaQuery<'Note'>>();
    expectTypeOf<ReturnType<SchemaQuery<'Folder'>['within']>>().toEqualTypeOf<SchemaQuery<'Folder'>>();
    expectTypeOf<ReturnType<SchemaQuery<'Note'>['withRole']>>().toEqualTypeOf<SchemaQuery<'Note'>>();
    expectTypeOf<ReturnType<SchemaQuery<'Note'>['forEach']>>().toEqualTypeOf<SchemaQuery<'Note'>>();
  });

  it('linkedWith, linkedTo and linkedFrom', () => {
    expectTypeOf<ReturnType<SchemaQuery<'Note'>['linkedWith']>>().toEqualTypeOf<SchemaQuery<'Note'>>();
    expectTypeOf<ReturnType<SchemaQuery<'Note'>['linkedTo']>>().toEqualTypeOf<SchemaQuery<'Note'>>();
    expectTypeOf<ReturnType<SchemaQuery<'Note'>['linkedFrom']>>().toEqualTypeOf<SchemaQuery<'Note'>>();
  });

  it('takes only the fields, kinds and roles the schema declares', () => {
    const check = () => {
      query('Note').where('title', 'x').has('pinned').lacks('body').orderBy('createdAt').withRole('starred');
      query('Note').linkedTo('contains');
      // @ts-expect-error a field the Note shape doesn't have
      query('Note').where('name', 'x');
      // @ts-expect-error a relation kind the schema doesn't declare
      query('Note').linkedTo('containz');
      // @ts-expect-error a role the schema doesn't declare
      query('Note').withRole('stared');
    };
    expectTypeOf(check).toBeFunction();
  });
});

// out and in walk to the entities at an edge's far end (the linked* steps filter in place), so the result holds the
// target type, not the type being queried from
describe('a walk moves to the far end\'s type', () => {
  it('out() and in() to the type they are given', () => {
    expectTypeOf<ReturnType<typeof folderBuilder.out<'Note'>>>().toEqualTypeOf<SchemaQuery<'Note'>>();
    expectTypeOf<ReturnType<typeof noteBuilder.in<'Folder'>>>().toEqualTypeOf<SchemaQuery<'Folder'>>();
  });

  it('to an untyped builder when given no type', () => {
    const check = () => expectTypeOf(query('Folder').out('contains')).toEqualTypeOf<SchemaQuery<string>>();
    expectTypeOf(check).toBeFunction();
  });
});

describe('the terminals answer ids tagged with the builder\'s type', () => {
  it('ids, first, last and groupBy', () => {
    expectTypeOf<ReturnType<SchemaQuery<'Note'>['ids']>>().toEqualTypeOf<EARS.EntityId<'Note'>[]>();
    expectTypeOf<ReturnType<SchemaQuery<'Note'>['first']>>().toEqualTypeOf<EARS.EntityId<'Note'> | undefined>();
    expectTypeOf<ReturnType<SchemaQuery<'Folder'>['last']>>().toEqualTypeOf<EARS.EntityId<'Folder'> | undefined>();
    expectTypeOf<ReturnType<SchemaQuery<'Folder'>['groupBy']>>().toEqualTypeOf<Map<unknown, EARS.EntityId<'Folder'>[]>>();
  });

  it('count a number and exists a boolean', () => {
    expectTypeOf<ReturnType<SchemaQuery<'Folder'>['count']>>().toBeNumber();
    expectTypeOf<ReturnType<SchemaQuery<'Note'>['exists']>>().toBeBoolean();
  });

  it('ids of an untyped builder stay plain', () => {
    expectTypeOf<ReturnType<SchemaQuery<string>['ids']>>().toEqualTypeOf<EARS.EntityId[]>();
  });

  it('a picked row holds the fields picked, typed by the shape', () => {
    const check = () => {
      const [row] = query('Note').pick(['title', 'pinned']);
      if (row) {
        expectTypeOf(row.title).toEqualTypeOf<string>();
        expectTypeOf(row.pinned).toEqualTypeOf<boolean | undefined>();
        expectTypeOf(row.id).toEqualTypeOf<EARS.EntityId<'Note'>>();
      }
      // @ts-expect-error a field the Folder shape doesn't have
      query('Folder').pick(['title']);
    };
    expectTypeOf(check).toBeFunction();
  });
});

describe('the default type parameters', () => {
  it('QueryBuilder without type arguments is the untyped builder', () => {
    expectTypeOf<QueryBuilder>().toEqualTypeOf<QueryBuilder<string, UntypedSchema>>();
  });

  it("an untyped builder's members answer untyped builders", () => {
    expectTypeOf<ReturnType<QueryBuilder['where']>>().toEqualTypeOf<QueryBuilder<string>>();
  });
});
