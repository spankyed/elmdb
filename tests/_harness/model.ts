// The correctness harness's reference model (tests/engine/model-based.spec.ts, tests/lmdb/crash.integration.spec.ts):
// what the engine's data should be after a sequence of writes, kept as plainly as a model can be, so the engine and its
// files are each compared against something with no cleverness in it
import type { EarsEngine } from '../../src/index.ts';

export type Value = string | number | number[];

/** One write the workload makes, as data: the model and the engine each apply it */
export type Op =
  | { op: 'create'; id: string; fields: Record<string, Value> }
  | { op: 'set'; id: string; key: string; value: Value }
  | { op: 'append'; id: string; key: string; value: number }
  | { op: 'delete'; id: string; key: string }
  | { op: 'link'; id: string; kind: string; target: string }
  | { op: 'unlink'; id: string; kind: string; target: string }
  | { op: 'destroy'; id: string }
  /** Writes made as one unit, which then returns or throws; a durable one commits flushed, which changes no data */
  | { op: 'unit'; ops: Op[]; throws: boolean; durable: boolean }
  /** The store refuses the next unit it is asked to commit, as a full disk would */
  | { op: 'refuseNext' };

/** The data as the model holds it, and as a snapshot of an engine reads */
export interface State {
  /** id → type and fields, the engine's own times left out */
  entities: Record<string, { type: string; fields: Record<string, unknown> }>;
  /** Each edge as `source|kind|target`, sorted */
  edges: string[];
}

const typeOf = (id: string) => id.slice(0, id.indexOf('-'));
const edge = (source: string, kind: string, target: string) => `${source}|${kind}|${target}`;

/** Why the model refuses a write, as the engine would; undefined when it is taken */
type Refusal = string | undefined;

/** A model of the engine's data: apply each op, and it says whether the engine must take it or refuse it */
export class Model {
  entities = new Map<string, { type: string; fields: Map<string, Value> }>();
  edges = new Set<string>();
  /** Set by `refuseNext`: the next write that reaches the store is refused */
  refusing = false;
  /** Whether the writes applied since it was last cleared reach the store (a builder write stamps `updatedAt` too) */
  private touched = false;

  clone(): Model {
    const copy = new Model();
    for (const [id, { type, fields }] of this.entities) copy.entities.set(id, { type, fields: new Map([...fields].map(([k, v]) => [k, Array.isArray(v) ? [...v] : v])) });
    copy.edges = new Set(this.edges);
    copy.refusing = this.refusing;
    return copy;
  }

  /** Applies one op outside any unit; returns whether the engine must refuse it, leaving the model as it was */
  apply(op: Op): Refusal {
    if (op.op === 'refuseNext') {
      this.refusing = true;
      return undefined;
    }
    const draft = this.clone();
    const refusal = op.op === 'unit' ? draft.unit(op) : draft.write(op, false);
    // A write that reaches no store (an edge linked again, one unlinked that was not there) cannot be refused by one
    const reached = !refusal && draft.touched;
    const refusedByStore = reached && this.refusing;
    if (reached) this.refusing = false;
    if (refusal || refusedByStore) return refusal ?? 'the store refused it';
    this.entities = draft.entities;
    this.edges = draft.edges;
    return undefined;
  }

  /** A unit: its writes in order, refused whole if one is refused, if it throws, or if it commits an edge to a missing end */
  private unit(op: Extract<Op, { op: 'unit' }>): Refusal {
    for (const inner of op.ops) {
      if (inner.op === 'unit' || inner.op === 'refuseNext') continue;
      const refusal = this.write(inner, true);
      if (refusal) return refusal;
    }
    if (op.throws) return 'the unit threw';
    for (const e of this.edges) {
      const [, , target] = e.split('|');
      if (!this.entities.has(target)) return `an edge to ${target}, which no entity has, at the unit's end`;
    }
    return undefined;
  }

  private write(op: Op, inUnit: boolean): Refusal {
    switch (op.op) {
      case 'create': {
        if (this.entities.has(op.id)) return `${op.id} is in use`;
        this.entities.set(op.id, { type: typeOf(op.id), fields: new Map(Object.entries(op.fields).map(([k, v]) => [k, Array.isArray(v) ? [...v] : v])) });
        this.touched = true;
        return undefined;
      }
      case 'set': case 'append': case 'delete': {
        const entity = this.entities.get(op.id);
        if (!entity) return `no entity has ${op.id}`;
        if (op.op === 'set') entity.fields.set(op.key, Array.isArray(op.value) ? [...op.value] : op.value);
        else if (op.op === 'delete') entity.fields.delete(op.key);
        else {
          const current = entity.fields.get(op.key);
          if (current !== undefined && !Array.isArray(current)) return `${op.key} is not a list`;
          entity.fields.set(op.key, [...(current ?? []), op.value]);
        }
        this.touched = true;
        return undefined;
      }
      case 'link': {
        if (!this.entities.has(op.id)) return `no entity has ${op.id}`;
        if (op.id === op.target) return 'a self link';
        // Outside a unit the target must exist now; inside one, when the unit commits
        if (!inUnit && !this.entities.has(op.target)) return `no entity has ${op.target}`;
        const added = edge(op.id, op.kind, op.target);
        if (!this.edges.has(added)) this.touched = true;
        this.edges.add(added);
        return undefined;
      }
      case 'unlink': {
        if (!this.entities.has(op.id)) return `no entity has ${op.id}`;
        if (this.edges.delete(edge(op.id, op.kind, op.target))) this.touched = true;
        return undefined;
      }
      case 'destroy': {
        if (!this.entities.has(op.id)) return `no entity has ${op.id}`;
        this.entities.delete(op.id);
        this.touched = true;
        for (const e of this.edges) {
          const [source, , target] = e.split('|');
          if (source === op.id || target === op.id) this.edges.delete(e);
        }
        return undefined;
      }
      default:
        return undefined;
    }
  }

  state(): State {
    const entities: State['entities'] = {};
    for (const id of [...this.entities.keys()].sort()) {
      const { type, fields } = this.entities.get(id)!;
      entities[id] = { type, fields: Object.fromEntries([...fields].sort(([a], [b]) => a.localeCompare(b))) };
    }
    return { entities, edges: [...this.edges].sort() };
  }
}

/** The engine's data in the model's terms: its own times left out, its relations as edges */
export function stateOf(engine: EarsEngine): State {
  const entities: State['entities'] = {};
  for (const id of [...engine.admin.getAllEntities()].sort()) {
    const { id: _id, entityType, createdAt: _c, updatedAt: _u, ...fields } = engine.getRow(id)!;
    entities[id] = { type: entityType, fields: Object.fromEntries(Object.entries(fields).sort(([a], [b]) => a.localeCompare(b))) };
  }
  const edges = engine.findRelations().map((r) => edge(r.source, r.kind, r.target)).sort();
  return { entities, edges };
}

/** Applies one op to an engine; whether it was refused (it threw), the store's refusals made through `refuseNext` */
export function applyTo(engine: EarsEngine, op: Op, refuseNext: () => void): boolean {
  const write = (inner: Op): void => {
    switch (inner.op) {
      case 'create': engine.create(typeOf(inner.id), { id: inner.id as never, ...inner.fields }); return;
      case 'set': engine.write(inner.id as never).set(inner.key, inner.value); return;
      case 'append': engine.write(inner.id as never).append(inner.key, inner.value); return;
      case 'delete': engine.write(inner.id as never).delete(inner.key); return;
      case 'link': engine.write(inner.id as never).link(inner.kind, inner.target as never); return;
      case 'unlink': engine.write(inner.id as never).unlink(inner.kind, inner.target as never); return;
      case 'destroy': engine.write(inner.id as never).destroy(); return;
      case 'unit':
        engine.transact(() => {
          for (const each of inner.ops) write(each);
          if (inner.throws) throw new Error('the unit threw');
        }, { durable: inner.durable });
        return;
      case 'refuseNext': refuseNext(); return;
    }
  };
  try {
    write(op);
    return false;
  } catch {
    return true;
  }
}
