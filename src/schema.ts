// The schema an engine's database holds, as apack's packs declare it, and the elmdb schema it becomes
// (`elmdb/docs/schema.md`). Whoever creates the engine builds it from what is registered — the app from its packs, a
// test from its own names — and changes it with `admin.setSchema` when that changes.
import type { RowRules } from './runtime.ts';
import { RESERVED_ATTRIBUTES } from './errors.ts';
import { setField } from './ids.ts';

/** What a relation kind is held to, as its declaration says */
export interface RelationRules {
  /** A link that would close a cycle through this kind is refused (`CYCLE`) */
  acyclic?: boolean;
  /** At most one edge of this kind from an entity to another; linking the pair again replaces the edge's info */
  unique?: boolean;
}

/** What an engine's database knows: its entity types, relation kinds and roles, each with its rules */
export interface EngineSchema {
  /** Each entity type, with what its rows are held to (`RowRules`) */
  types: Readonly<Record<string, RowRules>>;
  /** Each relation kind, with what its edges are held to */
  relations: Readonly<Record<string, RelationRules>>;
  roles: readonly string[];
}

/** elmdb's schema language, which a schema names */
export const ELMDB_SCHEMA_LANGUAGE = 'https://elmdb.dev/schema/1.json';

/** Whether elmdb writes the field itself, so a schema declares no field or index of it */
const isEngineField = (field: string) => (RESERVED_ATTRIBUTES as readonly string[]).includes(field);
/** Whether the field is one of the engine's an index may cover */
const isIndexableEngineField = (field: string) => field === 'createdAt' || field === 'updatedAt';

/**
 * The elmdb schema of `schema`. A type's fields are those its rules name: each required field, which may hold any value
 * including `null`, and each indexed field, which may be absent too; every other field is allowed, as a pack's shapes
 * are the compiler's to check. Each indexed field has a value index of its own, named after it; `role` and `shortCode`
 * are indexed by elmdb on every type. Every relation kind is many-to-many, and its edges may carry any info
 */
export function toElmdbSchema(schema: EngineSchema): object {
  // An entity type or field may be named `__proto__`, so every name is written with `setField`
  const types: Record<string, object> = {};
  for (const [type, rules] of Object.entries(schema.types)) {
    const fields: Record<string, object> = {};
    const indexes: Record<string, object> = {};
    for (const field of rules.indexes ?? []) {
      if (isEngineField(field) && !isIndexableEngineField(field)) continue;
      if (!isIndexableEngineField(field)) setField(fields, field, { type: 'any', optional: true, nullable: true });
      setField(indexes, field, { kind: 'value', fields: [field] });
    }
    for (const field of rules.required) {
      if (!isEngineField(field)) setField(fields, field, { type: 'any', nullable: true });
    }
    setField(types, type, {
      ...(rules.shortCode !== undefined && { shortCode: { prefix: rules.shortCode } }),
      fields,
      additionalFields: 'allow',
      indexes,
    });
  }
  const relations = Object.fromEntries(Object.entries(schema.relations).map(([kind, rules]) => [kind, {
    cardinality: 'many-to-many',
    ...(rules.acyclic && { acyclic: true }),
    ...(rules.unique && { unique: true }),
  }]));
  const roles = Object.fromEntries(schema.roles.map((role) => [role, {}]));
  return { $schema: ELMDB_SCHEMA_LANGUAGE, roles, relations, types };
}

/**
 * `held` (a database's elmdb schema, or `null`) with `declared`'s types, relation kinds and roles in place of its own of
 * the same names, and its others kept
 */
export function withDeclarations(held: unknown, declared: object): object {
  type Sections = { types?: object; relations?: object; roles?: object };
  const before = (held ?? {}) as Sections & Record<string, unknown>;
  const after = declared as Sections & Record<string, unknown>;
  return {
    ...before,
    ...after,
    types: { ...before.types, ...after.types },
    relations: { ...before.relations, ...after.relations },
    roles: { ...before.roles, ...after.roles },
  };
}

/** The names a database's elmdb schema declares, and the fields each type indexes */
export interface DeclaredNames {
  types: ReadonlySet<string>;
  relationKinds: ReadonlySet<string>;
  roles: ReadonlySet<string>;
  /** Per type, the fields a value index leads with */
  indexed: ReadonlyMap<string, ReadonlySet<string>>;
}

/** What a database's elmdb schema (its `schema`, canonical JSON, or `null` when it holds none) declares */
export function declaredNames(elmdbSchema: unknown): DeclaredNames {
  const schema = (elmdbSchema ?? {}) as {
    types?: Record<string, { indexes?: Record<string, { kind: string; fields?: unknown }> }>;
    relations?: Record<string, unknown>;
    roles?: Record<string, unknown>;
  };
  const indexed = new Map<string, ReadonlySet<string>>();
  for (const [type, def] of Object.entries(schema.types ?? {})) {
    const leading = Object.values(def.indexes ?? {})
      .flatMap((index) => (index.kind === 'value' && Array.isArray(index.fields) ? [String(index.fields[0])] : []));
    indexed.set(type, new Set(['role', 'shortCode', ...leading]));
  }
  return {
    types: new Set(Object.keys(schema.types ?? {})),
    relationKinds: new Set(Object.keys(schema.relations ?? {})),
    roles: new Set(Object.keys(schema.roles ?? {})),
    indexed,
  };
}
