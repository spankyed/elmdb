// What the engine holds of its elmdb database, and the one place it crosses to it: every read and write goes through
// `createConnection`, which sends them to the unit `transact` holds open, or to the database itself outside one, and
// hands back each answer decoded and each refusal as an `EarsError`.
//
// The port is structural so this module never loads `elmdb`: `@apackdev/ears/lmdb` opens the database, and only that
// entry imports it.
import { Packr, Unpackr } from 'msgpackr';
import { callSite, createCallTally, type CallCounting, type CallReport, type CallTally } from './calls.ts';
import { EarsError, type EarsErrorCode } from './errors.ts';
import { setField } from './ids.ts';
import { createOperationObservation, type EarsOperationObserver } from './observation.ts';

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
  /** What the unit has changed so far: msgpack of elmdb's `ChangeSet` */
  pendingChanges(): Uint8Array;
  /** `EarsDatabase.changedSince` read in the unit, before its own writes are stamped */
  changedSince(types: string[], from: Uint8Array, limit: number): Uint8Array;
  /** The sequence the unit commits as, which what it writes is stamped with */
  nextSequence(): number;
  /** Records `value` as the checkpoint `name`, committing with the unit or not at all */
  setCheckpoint(name: string, value: Uint8Array): void;
  /** The checkpoint `name` as the unit sees it */
  checkpoint(name: string): Uint8Array | null;
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
  /** Blocks until every index waiting to be built is built or has failed, and every rank index's pushes are done */
  waitForIndexBuilds(): void;
  /** Every entry removed but the database's format and schema */
  reset(): void;
  /** A compacted copy of one moment of the database into `target`, which opens as a database of its own */
  backup(target: string): void;
  /** Tells `listener` of each commit another process makes, having queued any index work it left owed. One listener; a second is refused */
  hearCommitsElsewhere(listener: () => void): void;
  /** The checkpoint `name` as the latest commit left it: a reader's place, kept with the rows it describes */
  checkpoint(name: string): Uint8Array | null;
  /** The epoch another process's replacement of every row moved the database to, when this handle hasn't adopted it. Until it does, commits here are refused (`DATA_REPLACED`) */
  replacedElsewhere(): number | null;
  /** Takes the replacement that moved the database to `epoch` as seen, so commits here are taken again; false, adopting nothing, when another has landed since */
  adoptReplacement(epoch: number): boolean;
  /** Whether another handle has committed since this one last asked, or since it opened; never for this handle's own */
  committedElsewhere(): boolean;
  /** The entities of types keeping a change order that changed after `from` (msgpack of a `ChangedPosition`): msgpack of elmdb's `Changed` */
  changedSince(types: string[], from: Uint8Array, limit: number): Uint8Array;
  /** Every entry replaced by another database's primary data, the derived data rebuilt under this one's schema */
  replaceWith(source: string): void;
  stats(): { types: Record<string, { live: number; trashed: number }>; relations: Record<string, number>; fileBytes: number };
  bulkLoad(loads: Uint8Array): void;
  /** The last commit's sequence, or 0: a number that only increases for as long as the database exists */
  readonly sequence: number;
  /** Tells `listener` of each commit the index builder makes that wrote into a tracked read; closing the database stops it */
  hearBuilderCommits(listener: (commit: { sequence: number; touched: number[] }) => void): void;
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

/** An answer as elmdb writes it: `kind` names it */
export type Answer = { kind: string } & Record<string, unknown>;

const PROTO = [...'__proto__'].map((char) => char.charCodeAt(0));

/** Whether `bytes` hold the text `__proto__` anywhere: a key of that name, or a value that merely spells it */
function spellsProto(bytes: Uint8Array): boolean {
  for (let at = bytes.indexOf(PROTO[0]!); at !== -1 && at + PROTO.length <= bytes.length; at = bytes.indexOf(PROTO[0]!, at + 1)) {
    if (PROTO.every((byte, offset) => bytes[at + offset] === byte)) return true;
  }
  return false;
}

/**
 * Every answer decoded alike, whatever its shape: exactly when its bytes could hold a `__proto__` key anywhere — a
 * row, a field's value, an edge's info, an answer inside another — and straight to objects otherwise
 */
function unpackAnswers(bytes: Uint8Array): Answer[] {
  return (spellsProto(bytes) ? objectsOf(exact.unpack(bytes)) : unpackr.unpack(bytes)) as Answer[];
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
  if (answer.kind === 'hits') for (const hit of answer.hits as Array<{ row?: unknown }>) row(hit.row);
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
  DUPLICATE_EDGE: 'DUPLICATE_EDGE',
  VALUE_INVALID: 'VALUE_INVALID',
  UNKNOWN_INDEX: 'UNKNOWN_INDEX',
  INDEX_BUILDING: 'INDEX_BUILDING',
  FIELD_TYPE: 'FIELD_TYPE',
  PATTERN_INVALID: 'PATTERN_INVALID',
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

/** Reports committed state before returning or rethrowing a failed sync. A refusal before commit does not notify.
 * A failed refresh after NOT_DURABLE preserves both errors and the committed-outcome code; ordinary refresh errors
 * propagate without being mistaken for operation failures. */
export function runCommitted<T>(operation: () => T, notify: () => void): T {
  let result: T;
  try {
    result = operation();
  } catch (error) {
    if ((error as { code?: unknown })?.code === 'NOT_DURABLE') {
      try { notify(); }
      catch (notification) {
        throw Object.assign(new AggregateError([error, notification],
          'The operation committed but failed to sync, and refreshing committed state also failed', { cause: error }),
        { code: 'NOT_DURABLE' });
      }
    }
    throw error;
  }
  notify();
  return result;
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

/** What an open unit has changed so far (elmdb's `ChangeSet`): the entities it wrote, by id */
export interface PendingChanges {
  entities: Record<string, unknown>;
}

/** Where a reader of what changed has read to: everything at or before `sequence`, and within it up to `after` */
export interface ChangedPosition {
  /** The last commit read in full */
  sequence: number;
  /** The last entity read within the next commit, when a page ended inside it */
  after?: { type: string; place: number };
}

/** What changed of some types after a position (`changedSince`) */
export interface ChangedEntities {
  /** The entities that changed and are still there, in the order of the commits that changed them */
  ids: string[];
  /** The entities destroyed since */
  destroyed: string[];
  /** Where to read from next */
  next: ChangedPosition;
  /** Whether this is all there is: when it is, `next` is the last commit's sequence */
  done: boolean;
  /** False when what changed can't all be known: the reader starts again from everything */
  complete: boolean;
}

/** What runs in a unit before it commits */
export interface BeforeCommit {
  /** Whether a write to an entity of this type has to run in a unit, so that `run` sees it */
  wants(type: string): boolean;
  /** Runs inside the unit: what it writes commits with it, and what it throws undoes it */
  run(): void;
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
  /** Tells the commit listeners of a commit no write of this connection made */
  heard(changes: { sequence: number; touched: number[] }): void;
  /** Tells the commit listeners of a commit made past the connection, whose writes aren't known key by key */
  committedEverything(): void;
  /**
   * Told that another process may have committed: asks the database, and if one did, the commit listeners hear
   * everything, since which keys isn't known. Answers whether one did.
   */
  changedElsewhere(): boolean;
  /** What the open unit has changed so far; none outside a unit */
  pendingChanges(): PendingChanges | undefined;
  /** What of `types` changed after `from`, read in the unit when one is open */
  changedSince(types: readonly string[], from: ChangedPosition, limit: number): ChangedEntities;
  /** The sequence the open unit commits as; none outside a unit */
  nextSequence(): number | undefined;
  /** The checkpoint `name`, read in the unit when one is open */
  checkpoint(name: string): unknown;
  /** Records `value` as the checkpoint `name` in the open unit, opening its write transaction as a write would */
  setCheckpoint(name: string, value: unknown): void;
  /** The epoch another process's replacement of every row moved the database to, when this connection hasn't adopted it */
  replacedElsewhere(): number | undefined;
  /** Takes that epoch as seen, its commits taken as news already acted on, and runs every watch again; false when another replacement has landed since */
  adoptReplacement(epoch: number): boolean;
  /** Sets what runs in a unit before it commits */
  beforeCommit(hook: BeforeCommit): void;
  /** Runs `fn` in a unit that what's set to run before a commit stays out of */
  transactWithoutHook<T>(fn: () => T): T;
  /** Calls `listener` soon after another process commits, starting this database hearing of that. Returns the unregistration */
  onFoundElsewhere(listener: () => void): () => void;
  /** The last commit's sequence, or 0 */
  lastCommit(): number;
  /** Counting the calls this connection makes, off until started */
  readonly calls: CallCounting;
  /** The count being kept, while counting runs: what the watch scheduler records each watch's runs into */
  tally(): CallTally | undefined;
  observe(observer: EarsOperationObserver): () => void;
}

export function createConnection(database: EarsDatabase, { freeze = false }: { freeze?: boolean } = {}): Connection {
  /**
   * The units `transact` is inside, outermost first. elmdb's unit opens at the first write, so a unit that writes
   * nothing commits nothing, and a read before it sees what a snapshot would; each level inside the outermost is a
   * savepoint of it, taken then or as the level begins, whichever is later. A level `kept` a write when one landed in
   * it or a level inside it that returned, so an outermost one that kept none aborts its unit rather than commit it
   */
  const levels: Array<{ savepoint?: number; kept?: boolean }> = [];
  let unit: EarsUnit | undefined;
  /** The token the reads being made are tracked under, while `track` runs */
  let tracking: number | undefined;
  const listeners = new Set<(commit: Commit) => void>();
  /** Calls every commit listener; a listener's throw is reported, not handed to the writer */
  const tellListeners = (commit: Commit) => {
    for (const listener of listeners) {
      try {
        listener(commit);
      } catch (error) {
        queueMicrotask(() => { throw error; });
      }
    }
  };
  const everyWatch = () => tellListeners({ sequence: database.sequence, touched: 'all' });
  /** A commit this connection made whose writes aren't known key by key */
  const committedEverything = everyWatch;
  const heard = (changes: { sequence: number; touched: number[] }) => {
    if (listeners.size > 0) tellListeners({ sequence: changes.sequence, touched: new Set(changes.touched) });
  };
  const foundElsewhere = new Set<() => void>();
  let hearing = false;
  const readChangedSince = (types: readonly string[], from: ChangedPosition, limit: number): ChangedEntities => {
    const position = pack(from);
    return unpackr.unpack(unit ? crossing(() => unit!.changedSince([...types], position, limit)) : database.changedSince([...types], position, limit)) as ChangedEntities;
  };
  let hook: BeforeCommit | undefined;
  let hookPaused = false;
  const typeOfOp = (op: Op): string | undefined => {
    const named = op as { type?: unknown; id?: unknown; source?: unknown };
    if (typeof named.type === 'string') return named.type;
    const id = named.id ?? named.source;
    return typeof id === 'string' ? id.slice(0, id.indexOf('-')) : undefined;
  };
  /** A write refused because another process replaced every row says the same as a wake, and a wake can be lost */
  const noticing = <T>(write: () => T): T => {
    try {
      return write();
    } catch (error) {
      if ((error as { code?: unknown })?.code === 'DATA_REPLACED') for (const listener of foundElsewhere) queueMicrotask(listener);
      throw error;
    }
  };
  /** Another process's commit found at a commit here, not yet answered by `changedElsewhere` */
  let foundUnasked = false;
  const made = (changes: { sequence: number; touched: number[] }) => {
    heard(changes);
    // A database that hears other processes wakes its listener itself when a commit here skips one of theirs. One that
    // can't hear them, as where nothing may listen, finds them at its own commits instead
    if (!hearing && foundElsewhere.size > 0 && database.committedElsewhere()) {
      foundUnasked = true;
      for (const listener of foundElsewhere) queueMicrotask(listener);
    }
  };

  const observation = createOperationObservation(
    () => ({ watch: tracking !== undefined, inUnit: levels.length > 0 }),
    () => database.sequence,
  );
  /** The count being kept while counting runs, and the unit it is counting until that commits or aborts */
  let tally: CallTally | undefined;
  let counted: { site: string; writes: number; ops: number; ms: number } | undefined;
  /** What `transact`'s outermost level counted, once its unit ends: a unit that wrote nothing opened none */
  const unitEnded = () => {
    if (tally && counted && counted.writes > 0) tally.record('unit', counted.site, false, counted.ops, counted.ms, counted.writes);
    counted = undefined;
  };
  /** Runs `fn`, adding its time to the unit being counted */
  const timedInUnit = <T>(fn: () => T): T => {
    const into = counted;
    if (!into) return fn();
    const start = performance.now();
    try {
      return fn();
    } finally {
      into.ms += performance.now() - start;
    }
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
    return observation.run('unit', 0, () => transactUnit(fn, options));
  }

  function transactUnit<T>(fn: () => T, options?: UnitOptions): T {
    const level: { savepoint?: number; kept?: boolean } = {};
    const outermost = levels.length === 0;
    if (outermost && tally) counted = { site: callSite(), writes: 0, ops: 0, ms: 0 };
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
      if (outermost && unit && hook && !hookPaused) hook.run();
    } catch (error) {
      levels.pop();
      const held = unit;
      if (outermost) {
        unit = undefined;
        if (held?.isOpen) timedInUnit(() => held.abort());
        unitEnded();
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
      if (level.kept) levels.at(-1)!.kept = true;
      return value;
    }
    unit = undefined;
    if (!held) {
      unitEnded();
      return value;
    }
    if (!level.kept) {
      // Every write it held was rolled back with a level that threw
      timedInUnit(() => held.abort());
      unitEnded();
      return value;
    }
    try {
      // The unit's deferred checks run as it commits; one that fails aborts it
      const changes = timedInUnit(() => unpackr.unpack(noticing(() => crossing(() => held.commit(options?.durable ?? false)))));
      unitEnded();
      made(changes as { sequence: number; touched: number[] });
    } catch (error) {
      const open = held.isOpen;
      if (open) held.abort();
      unitEnded();
      // A durable commit whose sync failed was made all the same, and its change set didn't cross with the error
      if (!open && (error as { code?: unknown })?.code === 'NOT_DURABLE') committedEverything();
      throw error;
    }
    return value;
  }

  /** A read's or a write's call, counted when counting runs */
  function counting<T>(kind: 'read' | 'write', ops: number, fn: () => T): T {
    if (!observation.active) return countedCall(kind, ops, fn);
    return observation.run(kind, ops, () => countedCall(kind, ops, fn));
  }

  function countedCall<T>(kind: 'read' | 'write', ops: number, fn: () => T): T {
    if (!tally) return fn();
    // A write inside a unit is the unit's, counted with it; one inside a unit begun before counting is not counted
    const inUnit = kind === 'write' && levels.length > 0;
    const site = inUnit ? undefined : callSite();
    const start = performance.now();
    try {
      return fn();
    } finally {
      const ms = performance.now() - start;
      if (site !== undefined) tally?.record(kind, site, tracking !== undefined, ops, ms);
      else if (counted) {
        counted.writes += 1;
        counted.ops += ops;
        counted.ms += ms;
      }
    }
  }

  /** The report `stop` answered, which `report` repeats until counting starts again */
  let stopped: CallReport | undefined;
  const calls: CallCounting = {
    start() {
      tally = createCallTally();
      stopped = undefined;
      // A unit begun before this count started isn't the count's, as one begun before counting at all isn't
      counted = undefined;
    },
    report: () => tally?.report(true) ?? stopped ?? createCallTally().report(false),
    stop() {
      stopped = (tally ?? createCallTally()).report(false);
      tally = undefined;
      counted = undefined;
      return stopped;
    },
  };

  const connection: Connection = {
    read(reads) {
      return counting('read', reads.length, () => {
        const bytes = observation.request(pack(tracking === undefined ? { reads } : { reads, track: tracking }));
        const held = unit;
        const answers = unpackAnswers(observation.response(crossing(() => (held ? held.read(bytes) : database.readSync(bytes)))));
        return freeze ? answers.map(freezeValues) : answers;
      });
    },
    write(ops, now = Date.now()) {
      // A write the hook has to see runs in a unit of its own. One it doesn't pays for this check and nothing else
      if (hook && levels.length === 0 && ops.some((op) => { const type = typeOfOp(op); return type !== undefined && hook!.wants(type); })) {
        return transact(() => connection.write(ops, now));
      }
      const written = counting('write', ops.length, () => {
        const bytes = observation.request(pack({ ops, now }));
        const target = levels.length ? writable() : undefined;
        const answer = observation.response(noticing(() => crossing(() => (target ? target.write(bytes) : database.write(bytes, false)))));
        if (target) levels.at(-1)!.kept = true;
        return unpackr.unpack(answer) as { results: OpResult[]; changes?: { sequence: number; touched: number[] } };
      });
      if (written.changes) made(written.changes);
      return written.results;
    },
    load(loads) {
      runCommitted(
        () => observation.run('load', loads.length, () => crossing(() => database.bulkLoad(observation.request(pack(loads))))),
        committedEverything,
      );
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
    heard,
    onCommit(listener) {
      listeners.add(listener);
      return () => { listeners.delete(listener); };
    },
    committedEverything,
    changedElsewhere() {
      const changed = database.committedElsewhere() || foundUnasked;
      foundUnasked = false;
      if (changed) everyWatch();
      return changed;
    },
    pendingChanges: () => (unit ? (unpackr.unpack(crossing(() => unit!.pendingChanges())) as PendingChanges) : undefined),
    changedSince: readChangedSince,
    // A unit opens its write transaction at its first write; asked first, it opens it now, as that write would
    nextSequence: () => (levels.length ? crossing(() => writable().nextSequence()) : undefined),
    checkpoint(name) {
      const bytes = unit ? crossing(() => unit!.checkpoint(name)) : database.checkpoint(name);
      return bytes ? unpackr.unpack(bytes) : undefined;
    },
    setCheckpoint(name, value) {
      if (!levels.length) throw new Error('A checkpoint is set inside a unit, with the work it records');
      const bytes = pack(value);
      crossing(() => writable().setCheckpoint(name, bytes));
      levels.at(-1)!.kept = true;
    },
    replacedElsewhere: () => database.replacedElsewhere() ?? undefined,
    adoptReplacement(epoch) {
      if (!database.adoptReplacement(epoch)) return false;
      // The replacement is news already acted on
      database.committedElsewhere();
      everyWatch();
      return true;
    },
    beforeCommit(given) { hook = given; },
    transactWithoutHook(fn) {
      hookPaused = true;
      try { return transact(fn); } finally { hookPaused = false; }
    },
    onFoundElsewhere(listener) {
      foundElsewhere.add(listener);
      if (!hearing) {
        database.hearCommitsElsewhere(() => { for (const each of foundElsewhere) each(); });
        hearing = true;
      }
      return () => { foundElsewhere.delete(listener); };
    },
    lastCommit: () => database.sequence,
    calls,
    tally: () => tally,
    observe: observation.observe,
  };
  return connection;
}
