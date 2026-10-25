import type { EARS } from './entities.ts';
import { idSuffix } from './utils.ts';

/**
 * Creates an id for a new entity of type `t`. A pack's `#generated/ears` exports this
 * typed to brand the id with the entity type, so reads through its helpers infer the shape.
 *
 * @internal
 */
export const newId = <E extends EARS.Entity>(t: E): EARS.EntityId =>
  `${t}-${idSuffix()}` as EARS.EntityId;

/**
 * An entity's type, from its id: the one record of it. `newId` builds `<Type>-<suffix>`, `create`
 * refuses an id naming another type, and the id is the wire's record too — a frontend opens an entity by
 * it, and reference nodes keep ids inside stored documents — so nothing else stores the type. The `entityType` field
 * every row carries is read from it.
 */
export const entityTypeOf = (id: EARS.EntityId) => {
  const dash = id.indexOf('-');
  return dash === -1 ? id as unknown as EARS.Entity : id.substring(0, dash) as EARS.Entity;
};

/**
 * Sets a field on an object a read builds: `__proto__` assigned would set the object's prototype rather than hold a
 * value, so it is defined as an own property
 */
export const setField = <V>(row: Record<string, V>, kind: string, value: V) => {
  if (kind === '__proto__') Object.defineProperty(row, kind, { value, writable: true, enumerable: true, configurable: true });
  else row[kind] = value;
};

export function getTimestamp(): number {
  return Date.now();
}
