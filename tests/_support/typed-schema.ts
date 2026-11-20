/**
 * A schema of two made-up entity types, and the typed facade over it, for the specs that hold EARS' types to what a
 * program sees: `Note`, numbered `NTE-<n>`, and `Folder`, which holds notes. The type-level specs read the facade's
 * types and never call it, so its engine getter throws: a spec that called a helper here would be testing nothing
 * this file can answer.
 */
import { defineEars, type BaseEntity, type EARS, type EarsSchema, type ShapeOf, type ShortCode } from '../../src/index.ts';

export interface NoteEntity extends BaseEntity {
  entityType: 'Note';
  title: string;
  body?: string;
  pinned?: boolean;
  tags?: string[];
  shortCode: ShortCode<'NTE'>;
}

export interface FolderEntity extends BaseEntity {
  entityType: 'Folder';
  name: string;
  color?: string;
}

export type Shapes = { Note: NoteEntity; Folder: FolderEntity };

/** The schema the facade checks against: both types' shapes and names, two relation kinds (one weighted), two roles and a rank index */
export interface Schema extends EarsSchema {
  shapes: Shapes;
  names: 'Note' | 'Folder';
  relKinds: 'contains' | 'cites';
  weightedKinds: 'cites';
  roles: 'starred' | 'archived';
  rankIndexes: 'noteRank';
}

/** An entity type's shape in the schema; an undeclared type reads as the base fields and `unknown` values */
export type EntityShape<E extends string> = ShapeOf<Shapes, E>;

export type { EARS };

export const { query, create, upsert, write, transact, newId, getAttr } = defineEars<Schema>(() => {
  throw new Error('the typed-schema facade is read for its types alone; no engine is bound to it');
});
