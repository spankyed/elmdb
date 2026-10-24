import type { AttributeStorage } from './attribute-storage.ts';
import { EARS } from './entities.ts';
import type { QueryBuilder, RelationEdge } from './runtime.ts';
import { EarsError } from './errors.ts';
import { SEQUENCE_TYPE } from './numbering.ts';
import { entityTypeOf, holds } from './attribute-storage.ts';

/** A query started from types and not yet walked */
interface TypeStart {
  types: readonly EARS.Entity[];
}

const ENTITY_TYPE = 'entityType' as EARS.AttrKind;
import type { TypeName } from './write.ts';
import type { NameChecks } from './names.ts';

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
 * `withTrashed` is the same entry points reaching those too; `trashed` the same entry points starting from only the
 * trashed entities.
 */
export interface Query extends QueryEntries {
  withTrashed: QueryEntries;
  trashed: QueryEntries;
}

/** An engine's untyped query entry points, over its storage (which reads the relation index for them) */
export function createQuery({ storage, isEntityType, names }: { storage: AttributeStorage; isEntityType: (name: string) => boolean; names: NameChecks }): Query {
  const {
    getAllEntities, getEntitiesOfType, hasEntity, isLive, isTrashed, holders, orderOf, removals, getAttr, hasAttr,
    findEntitiesInRelationTo, findEntitiesByRelationTo, relationsOf, getRow,
  } = storage;

  const prefixCache = new Map<EARS.Entity, (id: EARS.EntityId) => boolean>();
  const hasPrefix = (t: EARS.Entity) => {
    let fn = prefixCache.get(t);
    if (!fn) {
      fn = (id: EARS.EntityId) => id.startsWith(`${t}-`);
      prefixCache.set(t, fn);
    }
    return fn;
  };

  /** The types a start names; refuses a name that isn't a registered type */
  const ofTypes = (start: unknown): EARS.Entity[] => {
    const types = Array.isArray(start) ? start as unknown[] : [start];
    const unknown = types.filter((t) => typeof t !== 'string' || !isEntityType(t));
    if (unknown.length) {
      throw new EarsError(
        'UNKNOWN_ENTITY_TYPE',
        `query(${unknown.map((t) => JSON.stringify(t) ?? 'undefined').join(', ')}): not a registered entity type. ` +
          'query.ids(...) starts from ids, and query.all() from every entity.',
      );
    }
    return [...new Set(types as EARS.Entity[])];
  };

  // A value the value index answers for: a primitive, compared as `===` does (so not NaN), under a stored kind
  const indexable = (kind: EARS.AttrKind, v: unknown) =>
    kind !== ENTITY_TYPE && (v === null || ['string', 'number', 'boolean'].includes(typeof v)) && !Number.isNaN(v);

  const collator = new Intl.Collator();
  /**
   * Where a value sorts among the others in `orderBy`: booleans, numbers, dates, strings, then anything else by its
   * text, and last `null`, which `getAttr` gives for a missing value as for a stored one, and which
   * `orderBy(field, 'desc')` therefore puts first
   */
  const rankOf = (v: unknown): number =>
    v === null ? 5
      : typeof v === 'boolean' ? 0
      : typeof v === 'number' ? 1
      : v instanceof Date ? 2
      : typeof v === 'string' ? 3
      : 4;
  /** Two values of one rank, compared */
  const compareWithin = (rank: number, a: unknown, b: unknown): number => {
    switch (rank) {
      case 0: return Number(a) - Number(b);
      case 1: return (a as number) - (b as number);
      case 2: return (a as Date).getTime() - (b as Date).getTime();
      case 3: return collator.compare(a as string, b as string);
      case 4: return collator.compare(JSON.stringify(a), JSON.stringify(b));
      default: return 0;
    }
  };

  /**
   * The builder over the entities `alive` admits: every step after the start, a link's far end included, reaches
   * only those. A start from types stays unwalked until a step needs its ids, so `where(k, v)` and `withRole` on it
   * read the value index — the matches, rather than every entity of the types. Ids a chained step passes on were
   * checked when the step ran.
   */
  const builderOver = (alive: (id: EARS.EntityId) => boolean) => {
    const qxImpl = (source: EARS.EntityId[] | TypeStart): QueryBuilder<string> => {
      const resolvedAt = removals();
      const start = Array.isArray(source) ? undefined : source as TypeStart;
      let resolved = start ? undefined : source as EARS.EntityId[];
      const cur = () => (resolved ??= start!.types.flatMap((t) => getEntitiesOfType(t)).filter(alive));

      /** The ids holding `v` under `kind`, as `holds` would find them: from the index, in the order a walk gives */
      const holding = (kind: EARS.AttrKind, v: unknown): EARS.EntityId[] => {
        const found = holders(kind, v);
        if (!start) return cur().filter((i) => found.has(i));
        const rank = new Map(start.types.map((t, i) => [t, i]));
        const typeRank = (id: EARS.EntityId) => rank.get(entityTypeOf(id)) ?? -1;
        return [...found]
          .filter((id) => typeRank(id) !== -1 && alive(id))
          .sort((a, b) => typeRank(a) - typeRank(b) || orderOf(a) - orderOf(b));
      };

      // A builder may be kept across writes: a step after an entity left the engine drops the ids no longer in it
      const setIds = (next: EARS.EntityId[]) =>
        qxImpl(removals() === resolvedAt ? next : next.filter(alive));

      /** Whether an entity at an edge's far end is of one of the types, when any are given */
      const ofTypes = (type?: MaybeArr<EARS.Entity>) => {
        const types = type ? asArr(type) : [];
        return (id: EARS.EntityId) => !types.length || types.some((t) => hasPrefix(t)(id));
      };

      /** The entities at the far end of the set's edges of the kinds, out or in; never an entity of the set itself */
      const walk = (kinds: MaybeArr<string>, type: MaybeArr<EARS.Entity> | undefined, outward: boolean) => {
        names.relationKinds(kinds, outward ? 'out' : 'in');
        const matches = ofTypes(type);
        const reached = new Set<EARS.EntityId>();
        for (const from of cur()) for (const k of asArr(kinds)) {
          for (const i of findEntitiesByRelationTo(k, from, outward)) if (i !== from && matches(i)) reached.add(i);
        }
        // A relation's far end may not be in the engine, or be in the trash
        return qxImpl([...reached].filter(alive));
      };

      /**
       * Every entity the set reaches through edges of the kinds, out or in, any number deep: nearest first, each
       * once, never an entity of the set itself. The walk goes only through entities the query could reach, so a
       * cycle in the data ends it; `type` keeps the reached entities of the types and walks through the rest.
       */
      const reach = (kinds: MaybeArr<string>, type: MaybeArr<EARS.Entity> | undefined, outward: boolean) => {
        names.relationKinds(kinds, outward ? 'descendants' : 'ancestors');
        const matches = ofTypes(type);
        let frontier = cur();
        const seen = new Set(frontier);
        const reached: EARS.EntityId[] = [];
        while (frontier.length) {
          const next: EARS.EntityId[] = [];
          for (const from of frontier) for (const k of asArr(kinds)) {
            for (const i of findEntitiesByRelationTo(k, from, outward)) {
              if (seen.has(i) || !alive(i)) continue;
              seen.add(i);
              next.push(i);
            }
          }
          reached.push(...next);
          frontier = next;
        }
        return qxImpl(reached.filter(matches));
      };

      /**
       * The entities that have — or, with `keep` false, don't have — an edge of the kind going out (or coming in) whose
       * far end is `other`, or with none named, any entity this query could reach: the far end of a relation can be
       * missing or in the trash, as out and in step around
       */
      const filterByLink = (via: string, kind: string, other: EARS.EntityId | undefined, outward: boolean, keep: boolean) => {
        names.relationKinds(kind, via);
        const linked = (i: EARS.EntityId) => findEntitiesByRelationTo(kind, i, outward)
          .some((end) => (other === undefined ? end !== i && alive(end) : end === other));
        return setIds(cur().filter((i) => linked(i) === keep));
      };

      /** Each edge of the kinds, out or in, whose far end this query could reach: its relation row */
      const edges = <K extends string>(kinds: K | readonly K[], type: MaybeArr<EARS.Entity> | undefined, outward: boolean) => {
        names.relationKinds(kinds, outward ? 'outEdges' : 'inEdges');
        const matches = ofTypes(type);
        const rows: RelationEdge[] = [];
        for (const from of cur()) for (const k of asArr(kinds)) {
          for (const row of relationsOf(k, from, outward)) {
            const far = outward ? row.target : row.source;
            if (matches(far) && alive(far)) rows.push(row);
          }
        }
        return rows;
      };

      const self = {
        ofType: (t: EARS.Entity) => setIds(cur().filter(hasPrefix(t))),

        within: (sub: readonly EARS.EntityId[]) => {
          const subSet = new Set(sub);
          return setIds(cur().filter(i => subSet.has(i)));
        },

        where: (k: string, v: unknown) => {
          if (v === undefined) {
            throw new EarsError('NO_VALUE', `where('${k}'): no value to compare with. has('${k}') and lacks('${k}') ask whether there is one`);
          }
          const kind = EARS.AttrKind.Custom(k);
          if (indexable(kind, v)) return setIds(holding(kind, v));
          // A value matches itself or a list holding it; a predicate is given the whole value
          const test = typeof v === 'function'
            ? (i: EARS.EntityId) => hasAttr(i, kind) && (v as (value: unknown) => boolean)(getAttr(i, kind))
            : (i: EARS.EntityId) => holds(getAttr(i, kind), v);
          return setIds(cur().filter(test));
        },

        has: (k: string) => {
          const kind = EARS.AttrKind.Custom(k);
          return setIds(cur().filter(i => hasAttr(i, kind)));
        },

        lacks: (k: string) => {
          const kind = EARS.AttrKind.Custom(k);
          return setIds(cur().filter(i => !hasAttr(i, kind)));
        },

        withRole: (r: string) => {
          names.role(r, 'withRole');
          return setIds(holding(EARS.AttrKind.Role, r));
        },

        linkedWith: (other: EARS.EntityId) => {
          const linked = new Set(findEntitiesInRelationTo(other));
          return setIds(cur().filter(i => linked.has(i)));
        },

        linkedTo: (kind: string, other?: EARS.EntityId) => filterByLink('linkedTo', kind, other, true, true),
        linkedFrom: (kind: string, other?: EARS.EntityId) => filterByLink('linkedFrom', kind, other, false, true),
        unlinkedTo: (kind: string, other?: EARS.EntityId) => filterByLink('unlinkedTo', kind, other, true, false),
        unlinkedFrom: (kind: string, other?: EARS.EntityId) => filterByLink('unlinkedFrom', kind, other, false, false),

        out: (kinds: MaybeArr<string>, type?: MaybeArr<EARS.Entity>) => walk(kinds, type, true),
        in: (kinds: MaybeArr<string>, type?: MaybeArr<EARS.Entity>) => walk(kinds, type, false),
        descendants: (kinds: MaybeArr<string>, type?: MaybeArr<EARS.Entity>) => reach(kinds, type, true),
        ancestors: (kinds: MaybeArr<string>, type?: MaybeArr<EARS.Entity>) => reach(kinds, type, false),

        outEdges: <K extends string>(kinds: K | readonly K[], type?: MaybeArr<EARS.Entity>) => edges(kinds, type, true),
        inEdges: <K extends string>(kinds: K | readonly K[], type?: MaybeArr<EARS.Entity>) => edges(kinds, type, false),

        pick: <A extends readonly string[]>(fields: A) =>
          cur().map(i => {
            const o: Record<string, unknown> = { id: i };
            // A field the row lacks is left out, as getRow leaves it: a stored null reads as null
            fields.forEach(k => {
              if (k === "id") return;
              const kind = EARS.AttrKind.Custom(k);
              if (hasAttr(i, kind)) o[k] = getAttr(i, kind);
            });
            return o as { id: EARS.EntityId } & { [K in A[number]]: unknown };
          }),
        // The first row: the fields picked, or with none the whole row, as pickAll reads it
        pickOne: (f?: readonly string[]) => {
          if (f) return self.pick(f)[0];
          const [first] = cur();
          return first === undefined ? undefined : getRow(first)!;
        },

        // A step's ids are in the engine, so each has a row
        pickAll: () => cur().map((i) => getRow(i)!),

        orderBy: (
          field: string,
          dir: "asc" | "desc" = "asc",
        ) => {
          const kind = EARS.AttrKind.Custom(field);
          // Each value read once, then sorted by its type's rank and within it by value; a sort is stable, so ties
          // keep the query's order
          const ids = cur();
          const values = ids.map((id) => getAttr(id, kind));
          const ranks = values.map(rankOf);
          const order = ids.map((_, i) => i).sort((i, j) => {
            const res = ranks[i] - ranks[j] || compareWithin(ranks[i], values[i], values[j]);
            return dir === "asc" ? res : -res;
          });
          return setIds(order.map((i) => ids[i]));
        },

        reverse: () => setIds([...cur()].reverse()),

        limit: (n: number) => setIds(cur().slice(0, n)),

        distinct: (field?: string) => {
          let next: EARS.EntityId[];
          if (!field) {
            next = [...new Set(cur())];
          } else {
            const seen = new Set<unknown>();
            next = cur().filter(id => {
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
          cur().forEach(id => {
            const key = getAttr(id, EARS.AttrKind.Custom(field));
            const bucket = groups.get(key);
            if (bucket) {
              bucket.push(id);
            } else {
              groups.set(key, [id]);
            }
          });
          return groups;
        },

        ids: () => [...cur()],
        count: () => cur().length,
        first: () => cur()[0],
        last: () => cur().at(-1),
        exists: () => cur().length > 0,

        map: <T>(fn: (i: EARS.EntityId) => T) => cur().map(fn),
        forEach: (fn: (i: EARS.EntityId) => void) => (cur().forEach(fn), self),
        reduce: <T>(fn: (a: T, i: EARS.EntityId) => T, init: T) => cur().reduce(fn, init),
      } as const;

      return self as unknown as QueryBuilder<string>;
    };
    return qxImpl;
  };

  const engineOwn = [hasPrefix(EARS.Entity.Relation), hasPrefix(SEQUENCE_TYPE)];
  /** The entry points over the entities `alive` admits */
  const entriesOver = (alive: (id: EARS.EntityId) => boolean, start: (id: EARS.EntityId) => boolean = alive): QueryEntries => {
    const qx = builderOver(alive);
    const entries = ((types: unknown) => start === alive
      ? qx({ types: ofTypes(types) })
      : qx(ofTypes(types).flatMap((t) => getEntitiesOfType(t)).filter(start))) as unknown as QueryEntries;
    entries.ids = ((ids: EARS.EntityId | readonly EARS.EntityId[]) =>
      qx((Array.isArray(ids) ? ids : [ids]).filter(start))) as Query['ids'];
    entries.all = (() => qx(getAllEntities().filter((id) => start(id) && !engineOwn.some((own) => own(id))))) as Query['all'];
    return entries;
  };

  const entry = entriesOver(isLive) as Query;
  const withTrashed = entriesOver(hasEntity);
  entry.withTrashed = withTrashed;
  // The trash: entities that exist and are trashed, and the steps from them reach the trash too
  const trashed = entriesOver(hasEntity, isTrashed);
  entry.trashed = trashed;
  return entry;
}
