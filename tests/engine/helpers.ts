import { createMemoryKVBackend, createOrderedKV, type EARS, type KvBackend, type OrderedKV } from '../../src/index.ts';
import { freshEngine, type EngineUnderTest } from './engine-under-test.ts';

export const TYPES = new Set(['Task', 'Project', 'Person']);

export const isEntityType = (name: string) => TYPES.has(name);

export const KINDS = new Set(['about', 'assigned', 'blocks', 'child', 'contains', 'instance_of', 'lead', 'mentions', 'owner', 'owns', 'parent', 'peer', 'refers', 'refs', 'x']);

export const isRelationKind = (name: string) => KINDS.has(name);

export const ROLES = new Set(['current', 'focus', 'free', 'lead', 'member', 'owner', 'pinned']);

export const isRole = (name: string) => ROLES.has(name);

export type Id = EARS.EntityId;
export type RelId = EARS.RelationId;

/** An engine with the contract specs' entity types, relation kinds and roles, over a store in memory unless given one */
export function engine(kv?: OrderedKV): EngineUnderTest {
  return freshEngine({ isEntityType, isRelationKind, isRole, ...(kv && { kv }) });
}

/** A store in memory counting the units it committed: those that wrote, and returned */
export function committing(): { kv: OrderedKV; commits: () => number } {
  const backend = createMemoryKVBackend();
  let commits = 0;
  const counted: KvBackend = {
    read: (fn) => backend.read(fn),
    durable: () => backend.durable(),
    write(fn, options) {
      let wrote = false;
      const result = backend.write((w) => fn({
        ...w,
        put: (key, value) => { wrote = true; w.put(key, value); },
        del: (key) => { wrote = true; w.del(key); },
      }), options);
      if (wrote) commits++;
      return result;
    },
  };
  return { kv: createOrderedKV(counted), commits: () => commits };
}
