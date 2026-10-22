/** Why the engine refused a call: each names a claim the caller made that the engine could not hold it to */
export type EarsErrorCode =
  /** `create`, `upsert` or `qx` was given a name that is not a registered entity type */
  | 'UNKNOWN_ENTITY_TYPE'
  /** `write` was given an id no entity in the engine has */
  | 'NO_SUCH_ENTITY'
  /** `create` was given an id an entity already has */
  | 'ID_IN_USE'
  /** `upsert` or `create` named a type the id's entity doesn't have */
  | 'TYPE_MISMATCH'
  /** A write named an attribute the engine owns */
  | 'RESERVED_ATTRIBUTE'
  /** A `where` was given no value to compare with */
  | 'NO_VALUE';

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
 * Attributes only the engine writes: its own kinds (a role is written by `grant`, a relation's details by
 * `link`) and an entity's identity, type and creation time.
 */
export const RESERVED_ATTRIBUTES = ['id', 'entityType', 'createdAt', 'role', 'relationDetails'] as const;

/** Throws `RESERVED_ATTRIBUTE` for a name only the engine writes */
export function refuseReserved(name: string, via: string): void {
  if ((RESERVED_ATTRIBUTES as readonly string[]).includes(name)) {
    throw new EarsError(
      'RESERVED_ATTRIBUTE',
      `${via}('${name}'): "${name}" is written by the engine, not through ${via}. ` +
        (name === 'createdAt' ? 'Pass it to create(type, { createdAt }).'
          : name === 'role' ? 'Use grant() and revoke().'
            : name === 'relationDetails' ? 'Use link() and unlink().'
              : 'It is set when the entity is created.'),
    );
  }
}
