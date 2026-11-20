// An id's type tag, read by the compiler: these assertions fail `tsc` (the package's typecheck), not vitest, which only
// sees that each case was declared. Over `_support/typed-schema.ts`'s two types.
import { describe, expectTypeOf, it } from 'vitest';
import { create, getAttr, newId, query, typeOnly, write, type EARS, type EntityShape } from '../_support/typed-schema.ts';

declare const noteId: EARS.EntityId<'Note'>;
declare const plainId: EARS.EntityId;

describe('EntityId<E> tags an id with its type', () => {
  it("is an id, of an id's form, a type and a suffix; EntityId is EntityId<string>", () => {
    expectTypeOf<EARS.EntityId>().toEqualTypeOf<EARS.EntityId<string>>();
    expectTypeOf<EARS.EntityId<'Note'>>().toExtend<EARS.EntityId>();
    expectTypeOf<'note-123'>().toExtend<EARS.EntityId>();
    expectTypeOf<'note123'>().not.toExtend<EARS.EntityId>();
  });

  it("takes a plain id where a tagged one is expected, and neither another type's id nor a string", () => {
    const openNote = (id: EARS.EntityId<'Note'>) => id;
    openNote('note-1' as EARS.EntityId);
    // @ts-expect-error a Folder id where a Note id is expected
    openNote('folder-1' as EARS.EntityId<'Folder'>);
    // @ts-expect-error a string isn't an id
    openNote('note-1' as string);
  });

  it('lets a list of tagged ids be asked about plain ones', () => {
    const noteIds: EARS.EntityId<'Note'>[] = [];
    const plain = 'note-1' as EARS.EntityId;
    noteIds.includes(plain);
    new Set(noteIds).has(plain);
    new Map(noteIds.map((id) => [id, 1])).get(plain);
  });

  it("is no relation's: write refuses a relation id", () => typeOnly(() => {
    // @ts-expect-error write takes an entity's id; a relation is changed by updateRelation
    write('rel-1' as EARS.RelationId);
  }));
});

it('newId tags the id it makes with a declared type, not a runtime name, and refuses an undeclared literal', () => typeOnly(() => {
  expectTypeOf(newId('Note')).toEqualTypeOf<EARS.EntityId<'Note'>>();
  expectTypeOf(newId('Folder')).toEqualTypeOf<EARS.EntityId<'Folder'>>();
  expectTypeOf(newId('Note' as string)).toEqualTypeOf<EARS.EntityId>();
  // @ts-expect-error not an entity the schema declares
  newId('Unregistered');
}));

describe('a tagged id types what is read and written through it', () => {
  it("pickOne() answers that type's shape; an untagged id is typed by ofType, which checks the type", () => typeOnly(() => {
    expectTypeOf(query.ids(noteId).pickOne()).toEqualTypeOf<EntityShape<'Note'> | undefined>();
    expectTypeOf(query.ids(plainId).ofType('Folder').pickOne()).toEqualTypeOf<EntityShape<'Folder'> | undefined>();
  }));

  it("getAttr answers the field's type for a tagged id, and unknown for a plain one", () => typeOnly(() => {
    expectTypeOf(getAttr(noteId, 'title')).toEqualTypeOf<string | undefined>();
    expectTypeOf(getAttr(plainId, 'title')).toBeUnknown();
  }));

  it("write checks a field's value against the shape", () => typeOnly(() => {
    write(noteId).set('title', 'x').set('pinned', true).append('tags', 'y');
    // @ts-expect-error title is a string
    write(noteId).set('title', 1);
    // @ts-expect-error a field the Note shape doesn't have
    write(noteId).set('name', 'x');
    // @ts-expect-error the engine's own field
    write(noteId).set('createdAt', 1);
    // @ts-expect-error a required field is replaced with set, never deleted
    write(noteId).delete('title');
  }));

  it('create takes the row the shape requires, and answers a builder of that type', () => typeOnly(() => {
    expectTypeOf(create('Folder', { name: 'Inbox' }).id()).toEqualTypeOf<EARS.EntityId<'Folder'>>();
    // @ts-expect-error a Folder requires its name
    create('Folder', {});
    // @ts-expect-error a Folder requires a row
    create('Folder');
  }));

  it('a row read from a query carries its tagged id, without the shape being named again', () => typeOnly(() => {
    expectTypeOf(query('Note').pickAll()[0]!.id).toEqualTypeOf<EARS.EntityId<'Note'>>();
    expectTypeOf(query('Note').pickOne(['title'])!.id).toEqualTypeOf<EARS.EntityId<'Note'>>();
  }));
});
