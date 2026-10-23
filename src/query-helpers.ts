import type { Query } from './query.ts';
import type { EARS } from './entities.ts';

/**
 * An engine's finder, over its queries: the live row with an id, which reaches no entity in the trash (`getRow`
 * reads through it). Rows of a type, or of a type with a field's value, are a query: `query(type).pickAll()`,
 * `query(type).where(field, value).pickAll()`. Pack code gets it typed from its #generated/ears (defineEars).
 */
export function createQueryHelpers(query: Query) {
  function findById<T = unknown>(id: EARS.EntityId): T | undefined {
    return query.ids(id).pickAll()[0] as T | undefined;
  }

  return { findById };
}
