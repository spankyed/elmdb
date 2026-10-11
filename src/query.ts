import type { AttributeStorage } from './attribute-storage.ts';
import type { RelationIndexStore } from './relation-index.ts';
import { EARS } from './entities.ts';
import type { QueryBuilder } from './runtime.ts';
import { installedEngine } from './installed.ts';

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

export type QxStart =
  | EARS.EntityId
  | EARS.Entity
  | readonly EARS.Entity[]
  | readonly EARS.EntityId[];

/** An engine's untyped query entry point, over its storage and relation index */
export function createQx({ storage, relations, isEntityType }: { storage: AttributeStorage; relations: RelationIndexStore; isEntityType: (name: string) => boolean }): typeof qx {
  const {
    getAllEntities, getEntitiesOfType, hasEntity, removals, getAttr, getAttrs, getRoles,
    queryEntitiesInRelationTo, queryEntitiesByRelationTo, getAll,
  } = storage;
  const relationIndex = relations.index;

  const isEntity = (v: unknown): v is EARS.Entity =>
    typeof v === 'string' && isEntityType(v);

  const prefixCache = new Map<EARS.Entity, (id: EARS.EntityId) => boolean>();
  const hasPrefix = (t: EARS.Entity) => {
    let fn = prefixCache.get(t);
    if (!fn) {
      fn = (id: EARS.EntityId) => id.startsWith(`${t}-`);
      prefixCache.set(t, fn);
    }
    return fn;
  };

  // A start from the caller is resolved against the engine; `known` ids (a chained step's result) were
  // checked against it when the step ran
  const qxImpl = (start?: QxStart, known?: EARS.EntityId[]) => {
    const resolvedAt = removals();
    const resolveStart = (): EARS.EntityId[] => {
      if (start === undefined) return [...getAllEntities()];
      if (Array.isArray(start)) {
        if ((start as readonly unknown[]).every(isEntity)) {
          return (start as readonly EARS.Entity[]).flatMap(t => getEntitiesOfType(t));
        }
        return (start as readonly EARS.EntityId[]).filter(hasEntity);
      }
      if (typeof start !== 'string') return [];
      if (isEntity(start)) return [...getEntitiesOfType(start)];
      const id = start as EARS.EntityId;
      return hasEntity(id) ? [id] : [];
    };

    const ids: EARS.EntityId[] = known ?? resolveStart();

    // A builder may be kept across writes: a step after an entity left the engine drops the ids no longer in it
    const setIds = (next: EARS.EntityId[]) =>
      qxImpl(undefined, removals() === resolvedAt ? next : next.filter(hasEntity));

    const self = {
      ofType: (t: EARS.Entity) => setIds(ids.filter(hasPrefix(t))),

      inIds: (sub: readonly EARS.EntityId[]) => {
        const subSet = new Set(sub);
        return setIds(ids.filter(i => subSet.has(i)));
      },

      where: (k: EARS.AttrKind | string, v?: unknown) => {
        const kind = typeof k === "string" ? EARS.AttrKind.Custom(k) : k;
        const matches = v === undefined
          ? (i: EARS.EntityId) => getAttrs(i, kind).length > 0
          : (i: EARS.EntityId) => getAttrs(i, kind).some(attr => attr === v);
        return setIds(ids.filter(matches));
      },

      withRole: (r: string) => setIds(ids.filter(i => getRoles(i).includes(r))),

      relatedTo: (target: EARS.EntityId) => {
        const related = new Set(queryEntitiesInRelationTo(target));
        return setIds(ids.filter(i => related.has(i)));
      },

      related: (kind: string, other: EARS.EntityId, asSrc = false) => {
        const related = new Set(queryEntitiesByRelationTo(kind, other, asSrc));
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
          queryEntitiesByRelationTo(k, src, asSrc)
            .filter(matches)
            .forEach(i => {
              if (i !== src) out.add(i);
            });
        }
        // A relation's target may not be in the engine
        return qxImpl(undefined, [...out].filter(hasEntity));
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
          queryEntitiesByRelationTo(k, src, asSrc)
            .filter(matches)
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
            const picked = qxImpl(id).pickOne(fields);
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

      page: (size: number, cursor?: string | null) => {
        const start       = cursor ? b64Decode(cursor) : 0;
        const end         = start + size;
        const sliceIds    = ids.slice(start, end);
        const nextCursor  = end < ids.length ? b64Encode(end) : null;

        return { items: sliceIds, nextCursor } as const;
      },

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

    return self;
  };

  return ((start?: QxStart) => qxImpl(start)) as unknown as typeof qx;
}

/**
 * Query entry point: the entities it starts from.
 *
 * Overloads exist so an entity type threads its name into `QueryBuilder<E>`
 * (and from there into the shape registry), while id starts stay untyped. The
 * id overload precedes the entity-type one because `EntityId` is a template
 * literal and `EARS.Entity` is an open string.
 */
export function qx(): QueryBuilder<string>;
// A branded id carries its entity type, so it threads through like an entity-type start.
// An unbranded `EARS.EntityId` is `EntityId<string>`, which yields QueryBuilder<string>.
export function qx<E extends string>(start: EARS.EntityId<E>): QueryBuilder<E>;
export function qx<E extends string>(start: readonly EARS.EntityId<E>[]): QueryBuilder<E>;
export function qx<E extends EARS.Entity>(start: E): QueryBuilder<E>;
export function qx(start: readonly EARS.Entity[]): QueryBuilder<string>;
// Catch-all for starts that may be undefined at the call site.
export function qx(start?: QxStart): QueryBuilder<string>;
export function qx(start?: QxStart): QueryBuilder<string> {
  return installedEngine().qx(start);
}
