// The correctness harness's reference model (tests/engine/model-based.spec.ts, tests/lmdb/crash.integration.spec.ts):
// what the engine's data should be after a sequence of writes, kept as plainly as a model can be, so the engine and its
// files are each compared against something with no cleverness in it. It answers the derived reads too — a type's
// entities, its count and its trash, an index lookup, a role, an edge's far ends — since those are what the engine
// keeps in keys of their own, which can go stale while every row still reads right
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
  | { op: 'trash'; id: string }
  | { op: 'restore'; id: string }
  | { op: 'grant'; id: string; role: string }
  | { op: 'revoke'; id: string; role: string }
  /**
   * Writes made as one unit, which then returns or throws; a durable one commits flushed, which changes no data. A
   * unit inside another is caught there, so its refusal rolls back its own writes and the outer one goes on
   */
  | { op: 'unit'; ops: Op[]; throws: boolean; durable: boolean }
  /** The store refuses the next unit it is asked to commit, as a full disk would */
  | { op: 'refuseNext' };

/** One entity as the model holds it */
interface Entity {
  type: string;
  fields: Map<string, Value>;
  trashed: boolean;
  roles: string[];
}

/** The data as the model holds it, and as a snapshot of an engine reads */
export interface State {
  /** id → type, fields, whether it is trashed and its roles; the engine's own times left out */
  entities: Record<string, { type: string; fields: Record<string, unknown>; trashed: boolean; roles: string[] }>;
  /** Each edge as `source|kind|target`, sorted */
  edges: string[];
}

/** What the derived reads answer: every one in the order the engine gives it */
export type Derived = Record<string, readonly string[] | number>;

export const TYPES = ['Task', 'Note'] as const;
export const KINDS = ['refs', 'blocks'] as const;
export const ROLES = ['r1', 'r2'] as const;
/** The field every type indexes (`newEngine`'s row rules), and one none does, which a `where` filters for */
export const INDEXED = 'a';
export const UNINDEXED = 'b';
/** What a `where` asks for: every number the workload writes, alone or in a list, and two of its long strings */
export const PROBES: readonly unknown[] = [...Array.from({ length: 10 }, (_, n) => n), '0'.repeat(2100), '5'.repeat(2100)];

const typeOf = (id: string) => id.slice(0, id.indexOf('-'));
const edge = (source: string, kind: string, target: string) => `${source}|${kind}|${target}`;
const copy = (v: Value): Value => (Array.isArray(v) ? [...v] : v);
const holds = (value: unknown, v: unknown) => value === v || (Array.isArray(value) && value.includes(v as number));

/** Why the model refuses a write, as the engine would; undefined when it is taken */
type Refusal = string | undefined;

/** A model of the engine's data: apply each op, and it says whether the engine must take it or refuse it */
export class Model {
  /** In insertion order, which is the order a type's entities are read in */
  entities = new Map<string, Entity>();
  /** In the order they were linked, which is the order an edge's far ends are read in */
  edges = new Set<string>();
  /** Set by `refuseNext`: the next write that reaches the store is refused */
  refusing = false;
  /**
   * Whether the writes applied since it was last cleared reached the store: a write that succeeded, inside a unit
   * that rolled back included, since a unit writes through and undoes what it wrote
   */
  private touched = false;

  clone(): Model {
    const next = new Model();
    for (const [id, e] of this.entities) {
      next.entities.set(id, { type: e.type, fields: new Map([...e.fields].map(([k, v]) => [k, copy(v)])), trashed: e.trashed, roles: [...e.roles] });
    }
    next.edges = new Set(this.edges);
    next.refusing = this.refusing;
    return next;
  }

  /** Takes another model's data, as a unit that commits does */
  private adopt(other: Model): void {
    this.entities = other.entities;
    this.edges = other.edges;
  }

  /** Applies one op outside any unit; returns whether the engine must refuse it, leaving the model as it was */
  apply(op: Op): Refusal {
    if (op.op === 'refuseNext') {
      this.refusing = true;
      return undefined;
    }
    const draft = this.clone();
    const refusal = op.op === 'unit' ? draft.unit(op, true) : draft.write(op, false);
    // A write that reaches no store (an edge linked again, a role granted twice) cannot be refused by one
    const reached = !refusal && draft.touched;
    const refusedByStore = reached && this.refusing;
    if (reached) this.refusing = false;
    if (refusal || refusedByStore) return refusal ?? 'the store refused it';
    this.adopt(draft);
    return undefined;
  }

  /**
   * A unit: its writes in order, refused whole if one is refused or if it throws, and — the outermost only, where
   * the unit commits — if an edge it leaves has a missing end. A unit inside it that is refused rolls back alone
   */
  private unit(op: Extract<Op, { op: 'unit' }>, outermost: boolean): Refusal {
    for (const inner of op.ops) {
      if (inner.op === 'refuseNext') continue;
      if (inner.op === 'unit') {
        const nested = this.clone();
        const refused = nested.unit(inner, false);
        if (nested.touched) this.touched = true;
        if (!refused) this.adopt(nested);
        continue;
      }
      const refusal = this.write(inner, true);
      if (refusal) return refusal;
    }
    if (op.throws) return 'the unit threw';
    if (outermost) {
      for (const e of this.edges) {
        const [, , target] = e.split('|');
        if (!this.entities.has(target)) return `an edge to ${target}, which no entity has, at the unit's end`;
      }
    }
    return undefined;
  }

  private write(op: Op, inUnit: boolean): Refusal {
    if (op.op === 'unit' || op.op === 'refuseNext') return undefined;
    if (op.op === 'create') {
      if (this.entities.has(op.id)) return `${op.id} is in use`;
      this.entities.set(op.id, { type: typeOf(op.id), fields: new Map(Object.entries(op.fields).map(([k, v]) => [k, copy(v)])), trashed: false, roles: [] });
      this.touched = true;
      return undefined;
    }
    const entity = this.entities.get(op.id);
    if (!entity) return `no entity has ${op.id}`;
    switch (op.op) {
      case 'set': case 'append': case 'delete': {
        if (op.op === 'set') entity.fields.set(op.key, copy(op.value));
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
        if (op.id === op.target) return 'a self link';
        // Outside a unit the target must exist now; inside one, when the unit commits
        if (!inUnit && !this.entities.has(op.target)) return `no entity has ${op.target}`;
        const added = edge(op.id, op.kind, op.target);
        if (!this.edges.has(added)) this.touched = true;
        this.edges.add(added);
        return undefined;
      }
      case 'unlink': {
        if (this.edges.delete(edge(op.id, op.kind, op.target))) this.touched = true;
        return undefined;
      }
      case 'destroy': {
        this.entities.delete(op.id);
        this.touched = true;
        for (const e of this.edges) {
          const [source, , target] = e.split('|');
          if (source === op.id || target === op.id) this.edges.delete(e);
        }
        return undefined;
      }
      // Each writes nothing when it would change nothing, so the store is not reached
      case 'trash': case 'restore': {
        const trashed = op.op === 'trash';
        if (entity.trashed !== trashed) { entity.trashed = trashed; this.touched = true; }
        return undefined;
      }
      case 'grant': {
        if (!entity.roles.includes(op.role)) { entity.roles.push(op.role); this.touched = true; }
        return undefined;
      }
      case 'revoke': {
        if (entity.roles.includes(op.role)) { entity.roles = entity.roles.filter((r) => r !== op.role); this.touched = true; }
        return undefined;
      }
    }
  }

  state(): State {
    const entities: State['entities'] = {};
    for (const id of [...this.entities.keys()].sort()) {
      const { type, fields, trashed, roles } = this.entities.get(id)!;
      entities[id] = { type, fields: Object.fromEntries([...fields].sort(([a], [b]) => a.localeCompare(b))), trashed, roles: [...roles] };
    }
    return { entities, edges: [...this.edges].sort() };
  }

  /** The derived reads, as the engine should answer them (`derivedOf`) */
  derived(): Derived {
    const out: Derived = {};
    const live = (type: string) => [...this.entities].filter(([, e]) => e.type === type && !e.trashed).map(([id]) => id);
    for (const type of TYPES) {
      const ids = live(type);
      out[`${type} ids`] = ids;
      out[`${type} count`] = ids.length;
      out[`${type} trashed`] = [...this.entities].filter(([, e]) => e.type === type && e.trashed).map(([id]) => id);
      for (const role of ROLES) out[`${type} withRole ${role}`] = ids.filter((id) => this.entities.get(id)!.roles.includes(role));
      for (const field of [INDEXED, UNINDEXED]) {
        for (const probe of PROBES) {
          out[`${type} where ${field} ${String(probe).slice(0, 8)}`] = ids.filter((id) => holds(this.entities.get(id)!.fields.get(field), probe));
        }
      }
    }
    const isLive = (id: string) => this.entities.has(id) && !this.entities.get(id)!.trashed;
    for (const id of [...this.entities.keys()].filter(isLive)) {
      for (const kind of KINDS) {
        const ends = [...this.edges].map((e) => e.split('|'));
        out[`${id} out ${kind}`] = ends.filter(([s, k, t]) => s === id && k === kind && isLive(t)).map(([, , t]) => t);
        out[`${id} in ${kind}`] = ends.filter(([s, k, t]) => t === id && k === kind && isLive(s)).map(([s]) => s);
      }
    }
    return out;
  }
}

/** The engine's data in the model's terms: its own times left out, the trash a flag, its relations as edges */
export function stateOf(engine: EarsEngine): State {
  const entities: State['entities'] = {};
  for (const id of [...engine.admin.getAllEntities()].sort()) {
    const { id: _id, entityType, createdAt: _c, updatedAt: _u, trashedAt, role, ...fields } = engine.getRow(id)!;
    entities[id] = {
      type: entityType,
      fields: Object.fromEntries(Object.entries(fields).sort(([a], [b]) => a.localeCompare(b))),
      trashed: trashedAt !== undefined,
      roles: (role as string[] | undefined) ?? [],
    };
  }
  const edges = engine.findRelations().map((r) => edge(r.source, r.kind, r.target)).sort();
  return { entities, edges };
}

/** The derived reads, as the engine answers them, for the entities the model says are live */
export function derivedOf(engine: EarsEngine, model: Model): Derived {
  const out: Derived = {};
  for (const type of TYPES) {
    out[`${type} ids`] = engine.query(type).ids();
    out[`${type} count`] = engine.query(type).count();
    out[`${type} trashed`] = engine.query.trashed(type).ids();
    for (const role of ROLES) out[`${type} withRole ${role}`] = engine.query(type).withRole(role as never).ids();
    for (const field of [INDEXED, UNINDEXED]) {
      for (const probe of PROBES) out[`${type} where ${field} ${String(probe).slice(0, 8)}`] = engine.query(type).where(field, probe as never).ids();
    }
  }
  for (const [id, e] of model.entities) {
    if (e.trashed) continue;
    for (const kind of KINDS) {
      out[`${id} out ${kind}`] = engine.query.ids(id as never).out(kind as never).ids();
      out[`${id} in ${kind}`] = engine.query.ids(id as never).in(kind as never).ids();
    }
  }
  return out;
}

/** Applies one op to an engine; whether it was refused (it threw). A store's refusal is the backend's to arm */
export function applyTo(engine: EarsEngine, op: Op): boolean {
  const write = (inner: Op, nested: boolean): void => {
    switch (inner.op) {
      case 'create': engine.create(typeOf(inner.id), { id: inner.id as never, ...inner.fields }); return;
      case 'set': engine.write(inner.id as never).set(inner.key, inner.value); return;
      case 'append': engine.write(inner.id as never).append(inner.key, inner.value); return;
      case 'delete': engine.write(inner.id as never).delete(inner.key); return;
      case 'link': engine.write(inner.id as never).link(inner.kind, inner.target as never); return;
      case 'unlink': engine.write(inner.id as never).unlink(inner.kind, inner.target as never); return;
      case 'destroy': engine.write(inner.id as never).destroy(); return;
      case 'trash': engine.write(inner.id as never).trash(); return;
      case 'restore': engine.write(inner.id as never).restore(); return;
      case 'grant': engine.write(inner.id as never).grant(inner.role as never); return;
      case 'revoke': engine.write(inner.id as never).revoke(inner.role as never); return;
      case 'unit': {
        const run = () => engine.transact(() => {
          for (const each of inner.ops) write(each, true);
          if (inner.throws) throw new Error('the unit threw');
        }, { durable: inner.durable });
        // A unit inside another is caught there, which rolls back its writes and goes on
        if (nested) {
          try { run(); } catch { /* refused, and rolled back */ }
        } else run();
        return;
      }
      case 'refuseNext': return;
    }
  };
  try {
    write(op, false);
    return false;
  } catch {
    return true;
  }
}
