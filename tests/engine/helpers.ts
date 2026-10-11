import type { EARS, PersistenceSink } from '../../src/index.ts';
import { freshEngine, type EngineUnderTest } from './engine-under-test.ts';

export const TYPES = new Set(['Task', 'Project', 'Person', 'Relation']);

export const isEntityType = (name: string) => TYPES.has(name);

export type Id = EARS.EntityId;

/** An engine with the contract specs' entity types, and no persistence unless given */
export function engine(persistence?: PersistenceSink): EngineUnderTest {
  return freshEngine({ isEntityType, persistence });
}

export type SinkCall = [method: string, ...args: unknown[]];

/**
 * A sink recording each call with a copy of its arguments (the engine passes its live attribute
 * lists), and the ids in them replaced by `#1`, `#2`… in the order they first appear.
 */
export function recordingSink(): { sink: PersistenceSink; calls: SinkCall[] } {
  const calls: SinkCall[] = [];
  const names = new Map<string, string>();
  const rename = (value: unknown): unknown => {
    if (typeof value === 'string' && /^[A-Z][A-Za-z]*-[a-z0-9]{10,}$/.test(value)) {
      if (!names.has(value)) names.set(value, `#${names.size + 1}`);
      return names.get(value);
    }
    if (Array.isArray(value)) return value.map(rename);
    if (value && typeof value === 'object') return Object.fromEntries(Object.entries(value).map(([k, v]) => [k, rename(v)]));
    return value;
  };
  const record = (method: string) => (...args: unknown[]) => { calls.push([method, ...args.map((a) => rename(structuredClone(a)))]); };
  const sink: PersistenceSink = {
    onCreateEntity: record('onCreateEntity'),
    onDestroyEntity: record('onDestroyEntity'),
    onDropAttr: record('onDropAttr'),
    onPutAttrArray: record('onPutAttrArray'),
    onAddRelation: record('onAddRelation'),
    onUpdateRelation: record('onUpdateRelation'),
    onRemoveRelation: record('onRemoveRelation'),
  };
  return { sink, calls };
}
