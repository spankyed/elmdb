// The harness's engines: over the two types its workload writes, each indexing one field, and a backend that can
// refuse a commit
import { createKvEngine } from '../../src/kv/engine.ts';
import type { KvBackend, OrderedKV } from '../../src/kv.ts';
import { INDEXED, TYPES } from './model.ts';

const types = new Set<string>(TYPES);

export const newEngine = (kv: OrderedKV) => createKvEngine({
  kv, isEntityType: (name) => types.has(name), isRelationKind: () => true, isRole: () => true,
  rowRules: () => ({ required: [], indexes: [INDEXED] }),
});

/**
 * A backend that refuses the next unit that writes while it is told to, as a full disk would. It is told to only while
 * one of the workload's ops runs: a read builds an index in a unit of its own, which a full disk refuses too, and which
 * the model, knowing nothing of indexes, can't count
 */
export function refusable(backend: KvBackend): { backend: KvBackend; refusing: (on: boolean) => void } {
  let refusing = false;
  return {
    refusing: (on) => { refusing = on; },
    backend: {
      read: (fn) => backend.read(fn),
      durable: () => backend.durable(),
      write(fn, options) {
        return backend.write((writer) => {
          // A full disk refuses a commit that writes; an empty unit writes nothing to refuse
          let wrote = false;
          const result = fn({
            get: (key) => writer.get(key),
            scan: (...args) => writer.scan(...args),
            put: (key, value) => { wrote = true; writer.put(key, value); },
            del: (key) => { wrote = true; writer.del(key); },
            clear: () => { wrote = true; writer.clear(); },
          });
          if (refusing && wrote) {
            refusing = false;
            throw new Error('disk full');
          }
          return result;
        }, options);
      },
    },
  };
}
