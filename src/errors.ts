/** Why the engine refused a call: each names a claim the caller made that the engine could not hold it to */
export type EarsErrorCode =
  /** `create`, `upsert` or `query` was given a name that is not a registered entity type */
  | 'UNKNOWN_ENTITY_TYPE'
  /** `write` was given an id no entity in the engine has */
  | 'NO_SUCH_ENTITY'
  /** `updateRelation` or `removeRelation` was given an id no relation in the engine has */
  | 'NO_SUCH_RELATION'
  /** `create` was given an id an entity already has */
  | 'ID_IN_USE'
  /** `upsert` or `create` named a type the id's entity doesn't have */
  | 'TYPE_MISMATCH'
  /** A write named an attribute the engine owns */
  | 'RESERVED_ATTRIBUTE'
  /** `create` or `upsert` named one of the engine's own types, which its ids are written with (`RESERVED_ENTITY_TYPES`) */
  | 'RESERVED_ENTITY_TYPE'
  /** A `where` was given no value to compare with, or `set` none to store */
  | 'NO_VALUE'
  /** A list write (`append`, `removeAt`, `removeValue`) named an attribute whose value isn't a list */
  | 'NOT_A_LIST'
  /** A link, or a read or walk over links, named a relation kind that is not registered */
  | 'UNKNOWN_RELATION_KIND'
  /** A role write, or a query by role, named a role that is not registered */
  | 'UNKNOWN_ROLE'
  /** A created row lacked a field its type requires, or a delete named one */
  | 'REQUIRED_FIELD'
  /**
   * A created row brought a short code its type doesn't number, or one that isn't the type's `<prefix>-<n>`, or an
   * upsert's row one other than the entity's
   */
  | 'INVALID_SHORT_CODE'
  /** A created row brought a short code another entity holds */
  | 'SHORT_CODE_IN_USE'
  /** `link` named the entity it links from as its target */
  | 'SELF_LINK'
  /** `link` would close a cycle through a relation kind its declaration makes acyclic */
  | 'CYCLE'
  /** A write's value is one the database doesn't store: a function, a symbol, `undefined` in a list, or a number that isn't finite */
  | 'VALUE_INVALID';

/** An engine refusal, with its code */
export class EarsError extends Error {
  readonly code: EarsErrorCode;

  constructor(code: EarsErrorCode, message: string) {
    super(message);
    this.name = 'EarsError';
    this.code = code;
  }
}

/**
 * Attributes only the engine writes: its own kinds (a role is written by `grant`, when an entity was trashed by
 * `trash`) and an entity's identity, type, short code, creation time and last change, which the engine stamps on
 * every attribute write (`updatedAt`). A short code is numbered by `create` from the prefix the type's manifest
 * declares, and references name it, so nothing changes it once it is written.
 */
export const RESERVED_ATTRIBUTES = ['id', 'entityType', 'shortCode', 'createdAt', 'updatedAt', 'role', 'trashedAt'] as const;

/**
 * Entity types no pack declares: the engine's own ids are written with them — a relation's `Relation-<suffix>` — and
 * a relation is no entity a query reaches
 */
export const RESERVED_ENTITY_TYPES = { relation: 'Relation' } as const;

/** Throws `RESERVED_ATTRIBUTE` for a name only the engine writes */
export function refuseReserved(name: string, via: string): void {
  if ((RESERVED_ATTRIBUTES as readonly string[]).includes(name)) {
    throw new EarsError(
      'RESERVED_ATTRIBUTE',
      `${via}('${name}'): "${name}" is written by the engine, not through ${via}. ` +
        (name === 'createdAt' ? 'Pass it to create(type, { createdAt }).'
          : name === 'role' ? 'Use grant() and revoke().'
            : name === 'shortCode' ? 'create numbers it from the prefix the type declares; an import passes it to create(type, { shortCode }).'
              : 'It is set when the entity is created.'),
    );
  }
}
