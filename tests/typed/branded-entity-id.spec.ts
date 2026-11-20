// An id's type tag, read by the compiler: these assertions fail `tsc` (the package's typecheck), not vitest, which only
// sees that each type-level case was declared. Over `_support/typed-schema.ts`'s two types.
import { describe, expectTypeOf, it } from 'vitest';
import type { QueryBuilder } from '../../src/index.ts';
import { create, getAttr, newId, query, write, type EARS, type EntityShape, type Schema } from '../_support/typed-schema.ts';

describe('EntityId<E> tags an id with its type', () => {
  it('EntityId is EntityId<string>', () => {
    expectTypeOf<EARS.EntityId>().toEqualTypeOf<EARS.EntityId<string>>();
  });

  it('a tagged id is an id', () => {
    expectTypeOf<EARS.EntityId<'Note'>>().toMatchTypeOf<EARS.EntityId>();
  });

  it("keeps an id's form, a type and a suffix", () => {
    expectTypeOf<'note-123'>().toMatchTypeOf<EARS.EntityId>();
    expectTypeOf<'note123'>().not.toMatchTypeOf<EARS.EntityId>();
  });

  it("one type's id is not another's", () => {
    expectTypeOf<EARS.EntityId<'Note'>>().not.toEqualTypeOf<EARS.EntityId<'Folder'>>();
  });

  it("a plain id is taken where a tagged one is expected; another type's id is not, nor a string", () => {
    const openNote = (id: EARS.EntityId<'Note'>) => id;
    const plain = 'note-1' as EARS.EntityId;
    openNote(plain);
    // @ts-expect-error a Folder id where a Note id is expected
    openNote('folder-1' as EARS.EntityId<'Folder'>);
    // @ts-expect-error a string isn't an id
    openNote('note-1' as string);
  });

  it('a list of tagged ids is asked about plain ones', () => {
    const noteIds: EARS.EntityId<'Note'>[] = [];
    const plain = 'note-1' as EARS.EntityId;
    noteIds.includes(plain);
    new Set(noteIds).has(plain);
    new Map(noteIds.map((id) => [id, 1])).get(plain);
  });

  it("a relation's id is no entity's, and write refuses it", () => {
    const check = (relation: EARS.RelationId) => {
      // @ts-expect-error write takes an entity's id; a relation is changed by updateRelation
      write(relation);
    };
    expectTypeOf(check).toBeFunction();
  });
});

describe('newId tags the id it makes', () => {
  it('with a declared type', () => {
    expectTypeOf(newId('Note')).toEqualTypeOf<EARS.EntityId<'Note'>>();
    expectTypeOf(newId('Folder')).toEqualTypeOf<EARS.EntityId<'Folder'>>();
  });

  it('not with a name known only at runtime', () => {
    const entityType: string = 'Unregistered';
    expectTypeOf(newId(entityType)).toEqualTypeOf<EARS.EntityId>();
  });

  it('refuses a literal name the schema does not declare', () => {
    // @ts-expect-error not an entity the schema declares
    newId('Unregistered');
  });

  it('makes an id the query entry points take', () => {
    expectTypeOf<ReturnType<typeof newId<'Note'>>>().toMatchTypeOf<EARS.EntityId>();
  });
});

describe('a tagged id types what is read and written through it', () => {
  it("pickOne() answers that type's shape", () => {
    const row = (id: EARS.EntityId<'Note'>) => query.ids(id).pickOne();
    expectTypeOf(row).returns.toEqualTypeOf<EntityShape<'Note'> | undefined>();
  });

  it('an untagged id is typed by ofType, which checks the type rather than asserting the shape', () => {
    const row = (id: EARS.EntityId) => query.ids(id).ofType('Folder').pickOne();
    expectTypeOf(row).returns.toEqualTypeOf<EntityShape<'Folder'> | undefined>();
  });

  it("getAttr answers the field's type for a tagged id, and unknown for a plain one", () => {
    const check = (noteId: EARS.EntityId<'Note'>, plainId: EARS.EntityId) => {
      expectTypeOf(getAttr(noteId, 'title')).toEqualTypeOf<string | undefined>();
      expectTypeOf(getAttr(plainId, 'title')).toBeUnknown();
    };
    expectTypeOf(check).toBeFunction();
  });

  it("write checks a field's value against the shape for a tagged id", () => {
    const check = (noteId: EARS.EntityId<'Note'>) => {
      write(noteId).set('title', 'x').set('pinned', true).append('tags', 'y');
      // @ts-expect-error title is a string
      write(noteId).set('title', 1);
      // @ts-expect-error a field the Note shape doesn't have
      write(noteId).set('name', 'x');
      // @ts-expect-error the engine's own field
      write(noteId).set('createdAt', 1);
      // @ts-expect-error a required field is replaced with set, never deleted
      write(noteId).delete('title');
    };
    expectTypeOf(check).toBeFunction();
  });

  it('create takes the row the shape requires, and answers a builder of that type', () => {
    const check = () => {
      expectTypeOf(create('Folder', { name: 'Inbox' }).id()).toEqualTypeOf<EARS.EntityId<'Folder'>>();
      // @ts-expect-error a Folder requires its name
      create('Folder', {});
      // @ts-expect-error a Folder requires a row
      create('Folder');
    };
    expectTypeOf(check).toBeFunction();
  });
});

describe('an id from a query types the next read', () => {
  it("a type's builder answers its tagged ids", () => {
    expectTypeOf<ReturnType<QueryBuilder<'Note', Schema>['ids']>>().toEqualTypeOf<EARS.EntityId<'Note'>[]>();
  });

  it('without the shape being named again', () => {
    // Type-level only: never called, since no engine is bound to the facade
    const check = () => {
      const first = query('Note').first();
      if (first) expectTypeOf(query.ids(first).pickOne()).toEqualTypeOf<EntityShape<'Note'> | undefined>();
      const row = query('Note').pickAll()[0];
      if (row) expectTypeOf(row.id).toEqualTypeOf<EARS.EntityId<'Note'>>();
      const picked = query('Note').pickOne(['title']);
      if (picked) expectTypeOf(picked.id).toEqualTypeOf<EARS.EntityId<'Note'>>();
    };
    expectTypeOf(check).toBeFunction();
  });
});
