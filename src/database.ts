// What the engine holds of its elmdb database, and the one place it crosses to it: every read and write goes through
// `createConnection`, which sends them to the unit `transact` holds open, or to the database itself outside one, and
// hands back each answer decoded and each refusal as an `EarsError`.
//
// The port is structural so this module never loads `elmdb`: `@apackdev/ears/lmdb` opens the database, and only that
// entry imports it.
import { Packr, Unpackr } from 'msgpackr';
import { EarsError, type EarsErrorCode } from './errors.ts';
import { setField } from './ids.ts';

/** How `transact` commits */
export interface UnitOptions {
  /** On disk when `transact` returns, rather than with the database's next sync */
  durable?: boolean;
}

/** An elmdb unit held open across calls (`elmdb`'s `Unit`) */
export interface EarsUnit {
  write(batch: Uint8Array): Uint8Array;
  read(reads: Uint8Array): Uint8Array;
  savepoint(): number;
  rollbackTo(depth: number): void;
  release(depth: number): void;
  commit(durable?: boolean | null): Uint8Array;
  abort(): void;
  readonly isOpen: boolean;
}

/** An elmdb change to a schema: its path, what changed, and what was done about it */
export interface SchemaChange {
  path: string;
  change: string;
  effect: string;
}

/** What the engine needs of an open elmdb database (`elmdb`'s `Database`, which `@apackdev/ears/lmdb` opens) */
export interface EarsDatabase {
  readonly path: string;
  readonly readOnly: boolean;
  readonly isOpen: boolean;
  /** The schema the database holds, in elmdb's canonical JSON */
  readonly schema: unknown;
  write(batch: Uint8Array, durable?: boolean | null): Uint8Array;
  readSync(reads: Uint8Array): Uint8Array;
  begin(): EarsUnit;
  setSchema(schema: object): SchemaChange[];
  /** Every entry removed but the database's format and schema */
  reset(): void;
  /** A compacted copy of one moment of the database into `target`, which opens as a database of its own */
  backup(target: string): void;
  /** Every entry replaced by another database's primary data, the derived data rebuilt under this one's schema */
  replaceWith(source: string): void;
  stats(): { types: Record<string, { live: number; trashed: number }>; relations: Record<string, number>; fileBytes: number };
  bulkLoad(loads: Uint8Array): void;
  /** The last commit's sequence, or 0: a number that only increases for as long as the database exists */
  readonly sequence: number;
  /** Drops what the reads tracked under `token` touched (`{ reads, track: token }`) */
  untrack(token: number): void;
  /** How many tokens have tracked reads filed, and how many keys and ranges they hold */
  trackStats(): { owners: number; keys: number; ranges: number };
  durable(): Promise<void>;
  close(): void;
}

// A field holding `undefined` is absent, as it is once stored (msgpackr's `skipValues`, which its declarations omit);
// Dates are msgpack timestamps, which elmdb keeps
const packr = new Packr({ useRecords: false, skipValues: [undefined] } as ConstructorParameters<typeof Packr>[0]);
const unpackr = new Unpackr({ useRecords: false, mapsAsObjects: true, int64AsType: 'number' });
// The exact decode, for an answer whose maps may hold a `__proto__` key, which a decode straight to objects renames
const exact = new Unpackr({ useRecords: false, mapsAsObjects: false, int64AsType: 'number' });

export const pack = (value: unknown): Uint8Array => packr.pack(value);

/** Maps decoded as `Map`s, made objects whose keys are their own properties, `__proto__` among them */
function objectsOf(value: unknown): unknown {
  if (value instanceof Map) {
    const out: Record<string, unknown> = {};
    for (const [key, inner] of value) setField(out, String(key), objectsOf(inner));
    return out;
  }
  if (Array.isArray(value)) return value.map(objectsOf);
  return value;
}

/** An answer as elmdb writes it: `kind` names it, and `plainObjects` says whether its maps decode straight to objects */
export type Answer = { kind: string; plainObjects?: boolean } & Record<string, unknown>;

function unpackAnswers(bytes: Uint8Array): Answer[] {
  const answers = unpackr.unpack(bytes) as Answer[];
  if (answers.every((answer) => answer.plainObjects !== false)) return answers;
  return objectsOf(exact.unpack(bytes)) as Answer[];
}

/** `value` and everything inside it, frozen */
function deepFreeze<T>(value: T): T {
  if (value !== null && typeof value === 'object' && !Object.isFrozen(value)) {
    Object.freeze(value);
    for (const inner of Object.values(value as Record<string, unknown>)) deepFreeze(inner);
  }
  return value;
}

/**
 * The stored values an answer holds, frozen: each row's field values, a field's value and an edge's info. The rows and
 * lists a read builds are the caller's own to change, as they would be in the app
 */
function freezeValues(answer: Answer): Answer {
  const row = (r: unknown) => {
    if (r && typeof r === 'object') for (const value of Object.values(r)) deepFreeze(value);
  };
  if (answer.kind === 'visited') freezeValues(answer.answer as Answer);
  if (answer.kind === 'row') row(answer.row);
  if (answer.kind === 'rows') (answer.rows as unknown[]).forEach(row);
  if (answer.kind === 'field') deepFreeze(answer.value);
  if (answer.kind === 'relation' && answer.relation) deepFreeze((answer.relation as { info?: unknown }).info);
  if (answer.kind === 'relations') for (const relation of answer.relations as Array<{ info?: unknown }>) deepFreeze(relation.info);
  return answer;
}

/** elmdb's codes that are one of the engine's own, under the engine's name for it */
const CODES: Record<string, EarsErrorCode> = {
  UNKNOWN_TYPE: 'UNKNOWN_ENTITY_TYPE',
  RESERVED_FIELD: 'RESERVED_ATTRIBUTE',
  RESERVED_TYPE: 'RESERVED_ENTITY_TYPE',
  NO_SUCH_ENTITY: 'NO_SUCH_ENTITY',
  NO_SUCH_RELATION: 'NO_SUCH_RELATION',
  ID_IN_USE: 'ID_IN_USE',
  TYPE_MISMATCH: 'TYPE_MISMATCH',
  NOT_A_LIST: 'NOT_A_LIST',
  UNKNOWN_RELATION_KIND: 'UNKNOWN_RELATION_KIND',
  UNKNOWN_ROLE: 'UNKNOWN_ROLE',
  REQUIRED_FIELD: 'REQUIRED_FIELD',
  INVALID_SHORT_CODE: 'INVALID_SHORT_CODE',
  SHORT_CODE_IN_USE: 'SHORT_CODE_IN_USE',
  SELF_LINK: 'SELF_LINK',
  CYCLE: 'CYCLE',
  VALUE_INVALID: 'VALUE_INVALID',
  UNKNOWN_INDEX: 'UNKNOWN_INDEX',
};

/**
 * Runs `fn`, a call into elmdb, rethrowing a refusal the engine names as an `EarsError`; anything else elmdb throws
 * (a full disk, a closed database) is thrown as it is, with its own code
 */
export function crossing<T>(fn: () => T): T {
  try {
    return fn();
  } catch (error) {
    const code = (error as { code?: unknown })?.code;
    const mapped = typeof code === 'string' ? CODES[code] : undefined;
    if (mapped) throw new EarsError(mapped, (error as Error).message);
    throw error;
  }
}

/** One batch operation, as elmdb's protocol takes it (`elmdb/protocol/Op.ts`) */
export type Op = { op: string } & Record<string, unknown>;
/** One answer of a batch */
export type OpResult = { kind: 'done' } | { kind: 'entity'; id: string; shortCode: string | null; created: boolean }
  | { kind: 'relation'; id: string; linked: boolean } | { kind: 'label'; label: string } | { kind: 'stale' };

/**
 * A commit, as a commit listener hears it: its sequence, and the tokens whose tracked reads it wrote into, or `all` for
 * one whose writes aren't known key by key (an emptied or replaced database, a schema change, a load)
 */
export interface Commit {
  readonly sequence: number;
  readonly touched: ReadonlySet<number> | 'all';
}

/** The engine's crossing to its database: reads and writes, in the unit `transact` holds open or each its own */
export interface Connection {
  read(reads: ReadonlyArray<Record<string, unknown>>): Answer[];
  write(ops: readonly Op[], now?: number): OpResult[];
  /** Unchecked loads, one durable unit of their own (`Database.bulkLoad`); refused while a unit is open */
  load(loads: ReadonlyArray<Record<string, unknown>>): void;
  transact<T>(fn: () => T, options?: UnitOptions): T;
  /** Whether `transact` holds a unit open */
  readonly inUnit: boolean;
  readonly database: EarsDatabase;
  /** Runs `fn` with every read it makes tracked under `token`, adding what they touch to what the token holds */
  track<T>(token: number, fn: () => T): T;
  untrack(token: number): void;
  trackStats(): { keys: number; ranges: number };
  /**
   * Calls `listener` after each commit, outside the call that made it, with what it touched. Returns the
   * unregistration
   */
  onCommit(listener: (commit: Commit) => void): () => void;
  /** Tells the commit listeners of a commit made past the connection, whose writes aren't known key by key */
  committedEverything(): void;
  /** The last commit's sequence, or 0 */
  lastCommit(): number;
}

export function createConnection(database: EarsDatabase, { freeze = false }: { freeze?: boolean } = {}): Connection {
  /**
   * The units `transact` is inside, outermost first. elmdb's unit opens at the first write, so a unit that writes
   * nothing commits nothing, and a read before it sees what a snapshot would; each level inside the outermost is a
   * savepoint of it, taken then or as the level begins, whichever is later
   */
  const levels: Array<{ savepoint?: number }> = [];
  let unit: EarsUnit | undefined;
  /** The token the reads being made are tracked under, while `track` runs */
  let tracking: number | undefined;
  const listeners = new Set<(commit: Commit) => void>();
  /** Calls every commit listener; a listener's throw is reported, not handed to the writer */
  const committed = (commit: Commit) => {
    for (const listener of listeners) {
      try {
        listener(commit);
      } catch (error) {
        queueMicrotask(() => { throw error; });
      }
    }
  };
  const heard = (changes: { sequence: number; touched: number[] }) => {
    if (listeners.size > 0) committed({ sequence: changes.sequence, touched: new Set(changes.touched) });
  };

  /** The open unit, opened now for a write when none is */
  const writable = (): EarsUnit => {
    if (unit) return unit;
    const opened = crossing(() => database.begin());
    unit = opened;
    for (const level of levels.slice(1)) level.savepoint = crossing(() => opened.savepoint());
    return opened;
  };

  function transact<T>(fn: () => T, options?: UnitOptions): T {
    const level: { savepoint?: number } = {};
    const outermost = levels.length === 0;
    if (!outermost && unit) {
      const held = unit;
      level.savepoint = crossing(() => held.savepoint());
    }
    levels.push(level);
    let value: T;
    try {
      value = fn();
      if (value !== null && typeof value === 'object' && typeof (value as { then?: unknown }).then === 'function') {
        // The body has already started and cannot be stopped; what it does after its first await lands outside any
        // unit. Its own rejection is handled here so it isn't an unhandled one: the refusal is the report
        (value as unknown as PromiseLike<unknown>).then(undefined, () => {});
        throw new Error('transact() takes a synchronous function: an await inside it would split the unit');
      }
    } catch (error) {
      levels.pop();
      const held = unit;
      if (outermost) {
        unit = undefined;
        if (held?.isOpen) held.abort();
      } else if (held?.isOpen && level.savepoint !== undefined) {
        held.rollbackTo(level.savepoint);
      }
      throw error;
    }
    levels.pop();
    const held = unit;
    if (!outermost) {
      // A unit inside another commits with the outermost
      if (held && level.savepoint !== undefined) crossing(() => held.release(level.savepoint!));
      return value;
    }
    unit = undefined;
    if (!held) return value;
    try {
      // The unit's deferred checks run as it commits; one that fails aborts it
      heard(unpackr.unpack(crossing(() => held.commit(options?.durable ?? false))) as { sequence: number; touched: number[] });
    } catch (error) {
      if (held.isOpen) held.abort();
      throw error;
    }
    return value;
  }

  return {
    read(reads) {
      const bytes = pack(tracking === undefined ? { reads } : { reads, track: tracking });
      const held = unit;
      const answers = unpackAnswers(crossing(() => (held ? held.read(bytes) : database.readSync(bytes))));
      return freeze ? answers.map(freezeValues) : answers;
    },
    write(ops, now = Date.now()) {
      const bytes = pack({ ops, now });
      const target = levels.length ? writable() : undefined;
      const answer = crossing(() => (target ? target.write(bytes) : database.write(bytes, false)));
      const written = unpackr.unpack(answer) as { results: OpResult[]; changes?: { sequence: number; touched: number[] } };
      if (written.changes) heard(written.changes);
      return written.results;
    },
    load(loads) {
      crossing(() => database.bulkLoad(pack(loads)));
      committed({ sequence: database.sequence, touched: 'all' });
    },
    transact,
    get inUnit() { return levels.length > 0; },
    database,
    track(token, fn) {
      const outer = tracking;
      tracking = token;
      try {
        return fn();
      } finally {
        tracking = outer;
      }
    },
    untrack: (token) => database.untrack(token),
    trackStats: () => {
      const { keys, ranges } = database.trackStats();
      return { keys, ranges };
    },
    onCommit(listener) {
      listeners.add(listener);
      return () => { listeners.delete(listener); };
    },
    committedEverything: () => committed({ sequence: database.sequence, touched: 'all' }),
    lastCommit: () => database.sequence,
  };
}
