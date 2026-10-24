// The harness's engines: over the two types its workload writes, and a backend that can refuse a commit
import { createEarsEngine } from '../../src/index.ts';
import type { KvBackend, OrderedKV } from '../../src/kv.ts';

const TYPES = new Set(['Task', 'Note']);

export const newEngine = (kv: OrderedKV) =>
  createEarsEngine({ kv, isEntityType: (name) => TYPES.has(name), isRelationKind: () => true, isRole: () => true });

/** A backend that refuses the next unit that writes when told to, as a full disk would */
export function refusable(backend: KvBackend): { backend: KvBackend; refuseNext: () => void } {
  let refusing = false;
  return {
    refuseNext: () => { refusing = true; },
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
