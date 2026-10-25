import type { EARS, EarsDatabase, RelationRules, RowRules } from '../../src/index.ts';
import { freshEngine, type EngineUnderTest } from './engine-under-test.ts';

export const TYPES = new Set(['Task', 'Project', 'Person']);

export const isEntityType = (name: string) => TYPES.has(name);

export const KINDS = new Set(['about', 'assigned', 'blocks', 'child', 'contains', 'instance_of', 'lead', 'mentions', 'owner', 'owns', 'parent', 'peer', 'refers', 'refs', 'x']);

export const isRelationKind = (name: string) => KINDS.has(name);

export const ROLES = new Set(['current', 'focus', 'free', 'lead', 'member', 'owner', 'pinned']);

export const isRole = (name: string) => ROLES.has(name);

export type Id = EARS.EntityId;
export type RelId = EARS.RelationId;

/** The contract specs' entity types, relation kinds and roles, as `freshEngine` takes them */
export const NAMES = { types: TYPES, relationKinds: KINDS, roles: ROLES };

/** An engine with the contract specs' names, its rows and edges held to the rules given */
export function engine(rules: { rowRules?: Record<string, RowRules>; relationRules?: Record<string, RelationRules>; wrap?: (database: EarsDatabase) => EarsDatabase } = {}): EngineUnderTest {
  return freshEngine({ ...NAMES, ...rules });
}

/** A database wrapper counting the units committed: each write outside a unit, and each unit's commit */
export function committing(): { wrap: (database: EarsDatabase) => EarsDatabase; commits: () => number } {
  let commits = 0;
  const wrap = (database: EarsDatabase): EarsDatabase => new Proxy(database, {
    get(target, key) {
      if (key === 'write') {
        return (batch: Uint8Array, durable?: boolean) => {
          const answer = target.write(batch, durable);
          commits++;
          return answer;
        };
      }
      if (key === 'begin') {
        return () => {
          const unit = target.begin();
          return new Proxy(unit, {
            get(held, member) {
              if (member === 'commit') {
                return (durable?: boolean) => {
                  const changes = held.commit(durable);
                  commits++;
                  return changes;
                };
              }
              const value = Reflect.get(held, member, held);
              return typeof value === 'function' ? value.bind(held) : value;
            },
          });
        };
      }
      const value = Reflect.get(target, key, target);
      return typeof value === 'function' ? value.bind(target) : value;
    },
  });
  return { wrap, commits: () => commits };
}
