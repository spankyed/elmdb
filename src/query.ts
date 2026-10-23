import type { AttributeStorage } from './attribute-storage.ts';
import type { RelationIndexStore } from './relation-index.ts';
import { EARS } from './entities.ts';
import type { QueryBuilder } from './runtime.ts';
import { EarsError } from './errors.ts';
import { SEQUENCE_TYPE } from './entity-utils.ts';
import { holds } from './attribute-storage.ts';
import type { TypeName } from './write.ts';

type MaybeArr<T> = T | readonly T[];
function asArr<T>(v: MaybeArr<T>): readonly T[] {
  return (Array.isArray(v) ? v : [v]) as readonly T[];
}

export const b64Encode = (n: number) => btoa(String(n));
export const b64Decode = (s: string) => {
  try {
    const decoded = atob(s);
    const num = parseInt(decoded, 10);
    return isNaN(num) ? 0 : num;
  } catch {
    return 0;
  }
};

const liftOne = <F extends (...args: any[]) => any>(many: F) =>
  (...a: Parameters<F>) => (many as any)(...a)[0] ?? null;

/**
 * The query entry points over the entities they reach. `(type)` starts from every entity of a registered type, and
 * refuses a name that isn't one rather than finding nothing; `ids` starts from the ids given that exist; `all` from
 * every entity but the engine's own: the relations, which `findRelations` serves, and the numbering sequences.
 */
export interface QueryEntries {
  <E extends string>(type: TypeName<E>): QueryBuilder<E>;
  <E extends string>(types: readonly TypeName<E>[]): QueryBuilder<string>;
  ids<E extends string>(ids: EARS.EntityId<E> | readonly EARS.EntityId<E>[]): QueryBuilder<E>;
  all(): QueryBuilder<string>;
}

/**
 * The engine's queries. They reach no entity in the trash — not as a start, not at a link's far end — and
 * `withTrashed` is the same entry points reaching those too; `trashed(type)` starts from only the trashed
 * entities of a type.
 */
export interface Query extends QueryEntries {
  withTrashed: QueryEntries;
  trashed<E extends string>(type: TypeName<E>): QueryBuilder<E>;
}

/** An engine's untyped query entry points, over its storage and relation index */
export function createQuery({ storage, relations, isEntityType }: { storage: AttributeStorage; relations: RelationIndexStore; isEntityType: (name: string) => boolean }): Query {
  const {
    getAllEntities, getEntitiesOfType, hasEntity, isLive, isTrashed, removals, getAttr, hasAttr, getRoles,
    findEntitiesInRelationTo, findEntitiesByRelationTo, getAll,
  } = storage;
  const relationIndex = relations.index;

  const prefixCache = new Map<EARS.Entity, (id: EARS.EntityId) => boolean>();
  const hasPrefix = (t: EARS.Entity) => {
    let fn = prefixCache.get(t);
    if (!fn) {
      fn = (id: EARS.EntityId) => id.startsWith(`${t}-`);
      prefixCache.set(t, fn);
    }
    return fn;
  };

  /** Every entity of the types; refuses a name that isn't a registered type */
  const ofTypes = (start: unknown): EARS.EntityId[] => {
    const types = Array.isArray(start) ? start as unknown[] : [start];
    const unknown = types.filter((t) => typeof t !== 'string' || !isEntityType(t));
    if (unknown.length) {
      throw new EarsError(
        'UNKNOWN_ENTITY_TYPE',
        `query(${unknown.map((t) => JSON.stringify(t) ?? 'undefined').join(', ')}): not a registered entity type. ` +
          'query.ids(...) starts from ids, and query.all() from every entity.',
      );
    }
    return (types as EARS.Entity[]).flatMap((t) => getEntitiesOfType(t));
  };

  /**
   * The builder over the entities `alive` admits: every step after the start, a link's far end included, reaches
   * only those. `known` ids are a chained step's result, already checked when the step ran.
   */
  const builderOver = (alive: (id: EARS.EntityId) => boolean) => {
    const qxImpl = (known: EARS.EntityId[]): QueryBuilder<string> => {
      const resolvedAt = removals();
      const ids: EARS.EntityId[] = known;

      // A builder may be kept across writes: a step after an entity left the engine drops the ids no longer in it
      const setIds = (next: EARS.EntityId[]) =>
        qxImpl(removals() === resolvedAt ? next : next.filter(alive));

      const self = {
        ofType: (t: EARS.Entity) => setIds(ids.filter(hasPrefix(t))),

        inIds: (sub: readonly EARS.EntityId[]) => {
          const subSet = new Set(sub);
          return setIds(ids.filter(i => subSet.has(i)));
        },

        where: (k: string, v: unknown) => {
          if (v === undefined) {
            throw new EarsError('NO_VALUE', `where('${k}'): no value to compare with. has('${k}') and lacks('${k}') ask whether there is one`);
          }
          const kind = EARS.AttrKind.Custom(k);
          // A value matches itself or a list holding it; a predicate is given the whole value
          const test = typeof v === 'function'
            ? (i: EARS.EntityId) => hasAttr(i, kind) && (v as (value: unknown) => boolean)(getAttr(i, kind))
            : (i: EARS.EntityId) => holds(getAttr(i, kind), v);
          return setIds(ids.filter(test));
        },

        has: (k: string) => {
          const kind = EARS.AttrKind.Custom(k);
          return setIds(ids.filter(i => hasAttr(i, kind)));
        },

        lacks: (k: string) => {
          const kind = EARS.AttrKind.Custom(k);
          return setIds(ids.filter(i => !hasAttr(i, kind)));
        },

        withRole: (r: string) => setIds(ids.filter(i => getRoles(i).includes(r))),

        relatedTo: (target: EARS.EntityId) => {
          const related = new Set(findEntitiesInRelationTo(target));
          return setIds(ids.filter(i => related.has(i)));
        },

        related: (kind: string, other: EARS.EntityId, asSrc = false) => {
          const related = new Set(findEntitiesByRelationTo(kind, other, asSrc));
          return setIds(ids.filter(i => related.has(i)));
        },

        linksTo: (
          relKinds: MaybeArr<string>,
          tgtType?: MaybeArr<EARS.Entity>,
          asSrc = true,
        ) => {
          const kinds = asArr(relKinds);
          const targets = tgtType ? asArr(tgtType) : [];
          const matches = (id: EARS.EntityId) => !targets.length || targets.some(t => hasPrefix(t)(id));

          const out = new Set<EARS.EntityId>();
          for (const src of ids) for (const k of kinds) {
            findEntitiesByRelationTo(k, src, asSrc)
              .filter(matches)
              .forEach(i => {
                if (i !== src) out.add(i);
              });
          }
          // A relation's target may not be in the engine, or be in the trash
          return qxImpl([...out].filter(alive));
        },

        links: <K extends string>(
          relKinds: K | readonly K[],
          tgtType?: MaybeArr<EARS.Entity>,
          asSrc = true,
        ): Array<{ relation: K; id: EARS.EntityId }> => {
          const kinds = asArr(relKinds);
          const targets = tgtType ? asArr(tgtType) : [];
          const matches = (id: EARS.EntityId) => !targets.length || targets.some(t => hasPrefix(t)(id));

          const out: Array<{ relation: K; id: EARS.EntityId }> = [];
          for (const src of ids) for (const k of kinds)
            findEntitiesByRelationTo(k, src, asSrc)
              .filter(id => matches(id) && alive(id))
              .forEach(id => out.push({ relation: k as K, id }));

          return out;
        },

        edgeIds: (
          kinds?: string | readonly string[],
          asSrc = true,
        ): EARS.EntityId[] => {
          const ksInput = kinds ? asArr(kinds) : (Object.keys(relationIndex) as readonly string[]);
          const ks = ksInput.filter(k => relationIndex[k] !== undefined);
          const out = new Set<EARS.EntityId>();
          for (const i of ids) for (const k of ks) {
            const dir = relationIndex[k]!;
            (asSrc ? dir.bySource[i] : dir.byTarget[i])?.forEach(r => out.add(r));
          }
          return [...out];
        },

        pick: <A extends readonly string[]>(fields: A) =>
          ids.map(i => {
            const o: Record<string, unknown> = { id: i };
            fields.forEach(k => {
              if (k === "id") return;
              o[k] = getAttr(i, EARS.AttrKind.Custom(k));
            });
            return o as { id: EARS.EntityId } & { [K in A[number]]: unknown };
          }),
        pickOne: liftOne(function <A extends readonly string[]>(f: A) {
          return self.pick(f);
        }),

        pickAll: () => {
          return ids.map(i => {
            const o: Record<string, unknown> = { id: i };
            Object.assign(o, getAll(i));
            return o as { id: EARS.EntityId } & { [K in string]: unknown };
          });
        },

        linksPick: <K extends string, A extends readonly string[]>(
          relKinds: K | readonly K[],
          fields: A,
          tgtType?: MaybeArr<EARS.Entity>,
        ) => {
          const manyKinds = Array.isArray(relKinds) && relKinds.length > 1;
          return self.links(relKinds, tgtType)
            .map(({ relation, id }) => {
              const picked = qxImpl([id]).pickOne(fields);
              if (!picked) return null;
              return {
                ...(manyKinds ? { relation } : {}),
                ...picked,
              };
            })
            .filter(Boolean);
        },

        orderBy: (
          field: string,
          dir: "asc" | "desc" = "asc",
        ) => {
          const kind = EARS.AttrKind.Custom(field);
          const cmp = (a: unknown, b: unknown) => {
            if (a === b) return 0;
            if (a === undefined) return 1;
            if (b === undefined) return -1;
            if (typeof a === "number" && typeof b === "number") return a - b;
            return String(a).localeCompare(String(b));
          };
          const next = [...ids].sort((aId, bId) => {
            const res = cmp(getAttr(aId, kind), getAttr(bId, kind));
            return dir === "asc" ? res : -res;
          });
          return setIds(next);
        },

        reverse: () => setIds([...ids].reverse()),

        limit: (n: number) => setIds(ids.slice(0, n)),

        distinct: (field?: string) => {
          let next: EARS.EntityId[];
          if (!field) {
            next = [...new Set(ids)];
          } else {
            const seen = new Set<unknown>();
            next = ids.filter(id => {
              const val = getAttr(id, EARS.AttrKind.Custom(field));
              if (seen.has(val)) return false;
              seen.add(val);
              return true;
            });
          }
          return setIds(next);
        },

        groupBy: (field: string) => {
          const groups = new Map<unknown, EARS.EntityId[]>();
          ids.forEach(id => {
            const key = getAttr(id, EARS.AttrKind.Custom(field));
            const bucket = groups.get(key);
            if (bucket) {
              bucket.push(id);
            } else {
              groups.set(key, [id]);
            }
          });
          const result = new Map<unknown, QueryBuilder<string>>();
          groups.forEach((ids, key) => result.set(key, setIds(ids) as unknown as QueryBuilder<string>));
          return result;
        },

        ids: () => [...ids],
        id: () => self.first(),
        count: () => ids.length,
        first: () => ids[0] ?? null,
        last: () => (ids.length ? ids[ids.length - 1] : null),
        exists: () => ids.length > 0,

        map: <T>(fn: (i: EARS.EntityId) => T) => ids.map(fn),
        forEach: (fn: (i: EARS.EntityId) => void) => (ids.forEach(fn), self),
        reduce: <T>(fn: (a: T, i: EARS.EntityId) => T, init: T) => ids.reduce(fn, init),
      } as const;

      return self as unknown as QueryBuilder<string>;
    };
    return qxImpl;
  };

  const engineOwn = [hasPrefix(EARS.Entity.Relation), hasPrefix(SEQUENCE_TYPE)];
  /** The entry points over the entities `alive` admits */
  const entriesOver = (alive: (id: EARS.EntityId) => boolean): QueryEntries => {
    const qx = builderOver(alive);
    const entries = ((start: unknown) => qx(ofTypes(start).filter(alive))) as unknown as QueryEntries;
    entries.ids = ((start: EARS.EntityId | readonly EARS.EntityId[]) =>
      qx((Array.isArray(start) ? start : [start]).filter(alive))) as Query['ids'];
    entries.all = (() => qx(getAllEntities().filter((id) => alive(id) && !engineOwn.some((own) => own(id))))) as Query['all'];
    return entries;
  };

  const entry = entriesOver(isLive) as Query;
  const withTrashed = entriesOver(hasEntity);
  entry.withTrashed = withTrashed;
  const inTrash = builderOver(hasEntity);
  entry.trashed = ((type: unknown) => inTrash(ofTypes(type).filter(isTrashed))) as Query['trashed'];
  return entry;
}
