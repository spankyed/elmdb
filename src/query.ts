import type { AttributeStorage, TypeScope } from './attribute-storage.ts';
import { EARS } from './entities.ts';
import type { QueryBuilder, RelationEdge } from './runtime.ts';
import { EarsError } from './errors.ts';
import { holds, isIndexKey, setField } from './attribute-storage.ts';

/** A query started from types and not yet walked */
interface TypeStart {
  types: readonly EARS.Entity[];
}

/**
 * A visit of ids, one at a time: `visit` answers whether to go on, so a walk stops once it returns false. A loop
 * over a callback rather than a generator, which costs V8 an object and a resume per id.
 */
type Visit = (id: EARS.EntityId) => boolean;

/** A step not yet run: it runs `visit` over its ids, from the step before, each time they are asked for */
interface Stream {
  run: (visit: Visit) => void;
}

const ENTITY_TYPE = 'entityType' as EARS.AttrKind;
import type { TypeName } from './write.ts';
import type { NameChecks } from './names.ts';
import type { Relations } from './relations.ts';

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
 * every entity but the engine's own numbering sequences. A relation is not an entity: `findRelations` reads them.
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

/** An engine's untyped query entry points, over its attribute storage and its relations */
/** How many rows an unindexed `where` filters before the engine's creator is told (`warn`) the field wants an index */
export const UNINDEXED_WALK_WARNING = 2_000;

export function createQuery({ storage, relations, isEntityType, names, indexed, warn, snapshot }: {
  storage: AttributeStorage;
  /** Runs `fn` in one read snapshot (or the open unit): what each terminal reads in */
  snapshot: <T>(fn: () => T) => T;
  relations: Relations;
  isEntityType: (name: string) => boolean;
  names: NameChecks;
  /** Whether a `where` on the type's `kind` reads an index */
  indexed: (type: EARS.Entity, kind: EARS.AttrKind) => boolean;
  /** Told once per call site that a `where` filtered `UNINDEXED_WALK_WARNING` rows or more of a type without an index */
  warn?: (message: string) => void;
}): Query {
  const {
    getAllEntities, entitiesOfType, countOfType, hasEntity, isLive, isTrashed, holders, getAttr, hasAttr, getRow,
  } = storage;
  const { farEnds, edges: relationEdges, linkedWith: linkedWithOf } = relations;

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

  /**
   * Tells the engine's creator, once per call site, that a `where` filtered a type's rows for want of an index: the
   * site is the first frame outside this module's directory (`src/` or `dist/`, or the bundle it was built into), so
   * the message names the caller's code. The directory is read off this frame rather than `import.meta`, which a
   * CommonJS bundle leaves empty
   */
  const warnedSites = new Set<string>();
  const ownDir = /\(?((?:file:\/\/)?[^()\s]*?)[\\/][^\\/]+:\d+:\d+\)?$/.exec(new Error().stack?.split('\n')[1]?.trim() ?? '')?.[1];
  const own = (frame: string) => ownDir !== undefined && frame.includes(ownDir);
  const unindexed = (type: EARS.Entity, field: string, walked: number) => {
    if (!warn) return;
    // Deep enough to get past the engine's own frames — the store's layers take more than V8's default ten
    const limit = Error.stackTraceLimit;
    Error.stackTraceLimit = 50;
    const stack = new Error().stack;
    Error.stackTraceLimit = limit;
    const site = stack?.split('\n').slice(1).find((frame) => !own(frame))?.trim() ?? 'unknown';
    const key = `${type}\x1F${field}\x1F${site}`;
    if (warnedSites.has(key)) return;
    warnedSites.add(key);
    warn(`where('${field}') filtered ${walked} ${type} rows without an index (${site}). `
      + `Declare it in the ${type} entry of apack.json: "indexes": ["${field}"]`);
  };

  // A value an index holds (`isIndexKey`), under a stored kind: `entityType` is read from the id
  const indexable = (kind: EARS.AttrKind, v: unknown) => kind !== ENTITY_TYPE && isIndexKey(v);

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
   * only those. A builder is a recipe: no step runs when it is chained, and each terminal runs the whole query
   * against the data as it is then, so a builder kept across writes answers as one made after them, and a
   * predicate runs once per entity per terminal. A step that keeps or walks ids one at a time visits them as they
   * come: `first`, `exists`, `count` and `limit` stop once they have their answer. One that needs the whole set
   * (`orderBy`, `reverse`, `distinct`, `descendants`) collects it when it runs. A start from types stays unwalked,
   * so `where(k, v)` and `withRole` straight after it read the value index — the matches, rather than every
   * entity of the types. A start from types walks the range `scope` names, so a live query reads no trashed entity,
   * and its index reads keep what `admits` admits. Each terminal reads in one snapshot, and a callback it is given
   * (`map`, `forEach`, `reduce`) runs on the ids after the snapshot is released, so its own writes are its to read.
   */
  const builderOver = (alive: (id: EARS.EntityId) => boolean, scope: TypeScope, admits: (id: EARS.EntityId) => boolean = alive) => {
    const qxImpl = (source: TypeStart | Stream): QueryBuilder<string> => {
      const start = 'types' in source ? source : undefined;
      /**
       * Visits the ids: the start's walk, or the step's. Each step reads the data as it is when it runs, so every id
       * it passes on is one `alive` admits
       */
      const pull = (visit: Visit): void => {
        // The entities of the types in the engine's order, those `alive` admits
        if (start) {
          for (const t of start.types) if (!entitiesOfType(t, visit, scope)) return;
        } else (source as Stream).run(visit);
      };

      /** The ids, collected: what a terminal or step needing the whole set reads */
      const cur = (): EARS.EntityId[] => {
        const ids: EARS.EntityId[] = [];
        pull((id) => (ids.push(id), true));
        return ids;
      };

      /** The step keeping the ids `test` admits, made when the step runs (an index read, say) */
      const keepIds = (makeTest: () => (id: EARS.EntityId) => boolean) => qxImpl({
        run(visit) {
          const test = makeTest();
          pull((id) => !test(id) || visit(id));
        },
      });

      /** The step whose ids `compute` makes from the whole set before it, when it runs */
      const whole = (compute: (ids: EARS.EntityId[]) => Iterable<EARS.EntityId>) => qxImpl({
        run(visit) {
          for (const id of compute(cur())) if (!visit(id)) return;
        },
      });

      /**
       * The start's entities holding `v` under `kind`, as `test` finds them, in the order a walk of the types gives:
       * from a type's index where it declares one, and by filtering its rows where it doesn't
       */
      const holding = (field: string, kind: EARS.AttrKind, v: unknown, start: TypeStart, test: (id: EARS.EntityId) => boolean) => qxImpl({
        run(visit) {
          for (const type of start.types) {
            if (indexed(type, kind)) {
              const found = holders(type, kind, v).filter(admits);
              for (const id of found) if (!visit(id)) return;
              continue;
            }
            let walked = 0;
            const ended = entitiesOfType(type, (id) => {
              walked++;
              return !test(id) || visit(id);
            }, scope);
            if (walked >= UNINDEXED_WALK_WARNING) unindexed(type, field, walked);
            if (!ended) return;
          }
        },
      });

      /** Whether an entity at an edge's far end is of one of the types, when any are given */
      const ofTypes = (type?: MaybeArr<EARS.Entity>) => {
        const types = type ? asArr(type) : [];
        return (id: EARS.EntityId) => !types.length || types.some((t) => hasPrefix(t)(id));
      };

      /** The entities at the far end of the set's edges of the kinds, out or in; never an entity of the set itself */
      const walk = (kinds: MaybeArr<string>, type: MaybeArr<EARS.Entity> | undefined, outward: boolean) => {
        names.relationKinds(kinds, outward ? 'out' : 'in');
        const matches = ofTypes(type);
        const kindList = asArr(kinds);
        return qxImpl({
          run(visit) {
            const reached = new Set<EARS.EntityId>();
            pull((from) => {
              for (const k of kindList) {
                for (const i of farEnds(k, from, outward)) {
                  if (i === from || !matches(i) || reached.has(i)) continue;
                  reached.add(i);
                  // A relation's far end may not be in the engine, or be in the trash
                  if (alive(i) && !visit(i)) return false;
                }
              }
              return true;
            });
          },
        });
      };

      /**
       * Every entity the set reaches through edges of the kinds, out or in, any number deep: nearest first, each
       * once, never an entity of the set itself. The walk goes only through entities the query could reach, so a
       * cycle in the data ends it; `type` keeps the reached entities of the types and walks through the rest.
       */
      const reach = (kinds: MaybeArr<string>, type: MaybeArr<EARS.Entity> | undefined, outward: boolean) => {
        names.relationKinds(kinds, outward ? 'descendants' : 'ancestors');
        const matches = ofTypes(type);
        return whole((ids) => {
          let frontier = ids;
          const seen = new Set(frontier);
          const reached: EARS.EntityId[] = [];
          while (frontier.length) {
            const next: EARS.EntityId[] = [];
            for (const from of frontier) for (const k of asArr(kinds)) {
              for (const i of farEnds(k, from, outward)) {
                if (seen.has(i) || !alive(i)) continue;
                seen.add(i);
                next.push(i);
              }
            }
            reached.push(...next);
            frontier = next;
          }
          return reached.filter(matches);
        });
      };

      /**
       * The entities that have — or, with `keep` false, don't have — an edge of the kind going out (or coming in) whose
       * far end is `other`, or with none named, any entity this query could reach: the far end of a relation can be
       * missing or in the trash, as out and in step around
       */
      const filterByLink = (via: string, kind: string, other: EARS.EntityId | undefined, outward: boolean, keep: boolean) => {
        names.relationKinds(kind, via);
        const linked = (i: EARS.EntityId) => farEnds(kind, i, outward)
          .some((end) => (other === undefined ? end !== i && alive(end) : end === other));
        return keepIds(() => (i) => linked(i) === keep);
      };

      /** Each edge of the kinds, out or in, whose far end this query could reach: its relation row */
      const edges = <K extends string>(kinds: K | readonly K[], type: MaybeArr<EARS.Entity> | undefined, outward: boolean) => {
        names.relationKinds(kinds, outward ? 'outEdges' : 'inEdges');
        const matches = ofTypes(type);
        const rows: RelationEdge[] = [];
        snapshot(() => {
        for (const from of cur()) for (const k of asArr(kinds)) {
          for (const row of relationEdges(k, from, outward)) {
            const far = outward ? row.target : row.source;
            if (matches(far) && alive(far)) rows.push(row);
          }
        }
        });
        return rows;
      };

      /** One entity's fields: those it holds of `fields`, a stored null as null and one it lacks left out, as getRow does */
      const picked = (i: EARS.EntityId, fields: readonly string[]) => {
        const o: Record<string, unknown> = { id: i };
        for (const k of fields) {
          if (k === 'id') continue;
          const kind = EARS.AttrKind.Custom(k);
          if (hasAttr(i, kind)) setField(o, k, getAttr(i, kind));
        }
        return o;
      };

      const self = {
        ofType: (t: EARS.Entity) => keepIds(() => hasPrefix(t)),

        within: (sub: readonly EARS.EntityId[]) => {
          const subSet = new Set(sub);
          return keepIds(() => (i) => subSet.has(i));
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
          if (start && indexable(kind, v)) return holding(k, kind, v, start, test);
          return keepIds(() => test);
        },

        has: (k: string) => {
          const kind = EARS.AttrKind.Custom(k);
          return keepIds(() => (i) => hasAttr(i, kind));
        },

        lacks: (k: string) => {
          const kind = EARS.AttrKind.Custom(k);
          return keepIds(() => (i) => !hasAttr(i, kind));
        },

        withRole: (r: string) => {
          names.role(r, 'withRole');
          const test = (i: EARS.EntityId) => holds(getAttr(i, EARS.AttrKind.Role), r);
          if (start) return holding(EARS.AttrKind.Role, EARS.AttrKind.Role, r, start, test);
          return keepIds(() => test);
        },

        linkedWith: (other: EARS.EntityId) => keepIds(() => {
          const linked = new Set(linkedWithOf(other));
          return (i) => linked.has(i);
        }),

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
          snapshot(() => cur().map((i) => picked(i, fields) as { id: EARS.EntityId } & { [K in A[number]]: unknown })),
        // The first row: the fields picked, or with none the whole row, as pickAll reads it
        pickOne: (f?: readonly string[]) => snapshot(() => {
          const first = self.first();
          if (first === undefined) return undefined;
          return f ? picked(first, f) : getRow(first)!;
        }),

        // A step's ids are in the engine, so each has a row
        pickAll: () => snapshot(() => cur().map((i) => getRow(i)!)),

        orderBy: (
          field: string,
          dir: "asc" | "desc" = "asc",
        ) => {
          const kind = EARS.AttrKind.Custom(field);
          // Each value read once, then sorted by its type's rank and within it by value; a sort is stable, so ties
          // keep the query's order
          return whole((ids) => {
            const values = ids.map((id) => getAttr(id, kind));
            const ranks = values.map(rankOf);
            const order = ids.map((_, i) => i).sort((i, j) => {
              const res = ranks[i] - ranks[j] || compareWithin(ranks[i], values[i], values[j]);
              return dir === "asc" ? res : -res;
            });
            return order.map((i) => ids[i]);
          });
        },

        reverse: () => whole((ids) => ids.reverse()),

        // The first `n` ids, `n` rounded down: none for a count below one, or one that isn't a number
        limit: (n: number) => {
          const take = Number.isNaN(n) ? 0 : Math.floor(n);
          return qxImpl({
            run(visit) {
              if (take <= 0) return;
              let taken = 0;
              pull((id) => visit(id) && ++taken < take);
            },
          });
        },

        distinct: (field?: string) => whole((ids) => {
          if (!field) return new Set(ids);
          const seen = new Set<unknown>();
          return ids.filter((id) => {
            const val = getAttr(id, EARS.AttrKind.Custom(field));
            if (seen.has(val)) return false;
            seen.add(val);
            return true;
          });
        }),

        groupBy: (field: string) => snapshot(() => {
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
        }),

        ids: () => snapshot(cur),
        count: () => snapshot(() => {
          // A type's live entities, counted as they enter and leave: one read, where any step would need the walk
          if (start && scope === 'live') return start.types.reduce((total, t) => total + countOfType(t), 0);
          let n = 0;
          pull(() => (n++, true));
          return n;
        }),
        first: (): EARS.EntityId | undefined => snapshot(() => {
          let first: EARS.EntityId | undefined;
          pull((id) => ((first = id), false));
          return first;
        }),
        last: () => snapshot(cur).at(-1),
        exists: () => self.first() !== undefined,

        map: <T>(fn: (i: EARS.EntityId) => T) => snapshot(cur).map(fn),
        forEach: (fn: (i: EARS.EntityId) => void) => (snapshot(cur).forEach(fn), self),
        reduce: <T>(fn: (a: T, i: EARS.EntityId) => T, init: T) => snapshot(cur).reduce(fn, init),
      } as const;

      return self as unknown as QueryBuilder<string>;
    };
    return qxImpl;
  };

  /** The entry points over the entities `alive` admits, starting from those `start` admits, a type's from its `scope` */
  const entriesOver = (alive: (id: EARS.EntityId) => boolean, scope: TypeScope, start: (id: EARS.EntityId) => boolean = alive): QueryEntries => {
    const qx = builderOver(alive, scope, start);
    /** The ids `list` gives that `keep` admits, read when the query runs */
    const reading = (list: () => readonly EARS.EntityId[], keep: (id: EARS.EntityId) => boolean) => qx({
      run(visit) {
        for (const id of list()) if (keep(id) && !visit(id)) return;
      },
    });
    const entries = ((types: unknown) => {
      const named = ofTypes(types);
      return qx({ types: named });
    }) as unknown as QueryEntries;
    // The ids named: each checked when the query runs, so one that has left the engine (or the trash) is skipped
    entries.ids = ((ids: EARS.EntityId | readonly EARS.EntityId[]) => {
      const named = Array.isArray(ids) ? [...ids] : [ids as EARS.EntityId];
      return reading(() => named, start);
    }) as Query['ids'];
    entries.all = (() => reading(getAllEntities, start)) as Query['all'];
    return entries;
  };

  const entry = entriesOver(isLive, 'live') as Query;
  const withTrashed = entriesOver(hasEntity, 'all');
  entry.withTrashed = withTrashed;
  // The trash: entities that exist and are trashed, and the steps from them reach the trash too
  const trashed = entriesOver(hasEntity, 'trashed', isTrashed);
  entry.trashed = trashed;
  return entry;
}
