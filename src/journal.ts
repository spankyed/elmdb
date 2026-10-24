// What makes `transact` a unit: its writes run in one write unit of the store, which keeps nothing of them when it
// throws (`kv.ts`), and the persistence calls they make are held until the outermost unit commits.
import type { PersistenceSink } from './runtime.ts';
import type { StoreAccess } from './store-access.ts';

/** One open unit: where its sink calls start, and its checks */
interface Level {
  sinkMark: number;
  checks: Array<() => void>;
}

/** An engine's unit of work, as the writes see it */
export interface Journal {
  /** Whether a unit is open */
  readonly open: boolean;
  /**
   * Runs `check` when the outermost unit commits, before anything reaches the sink — at once outside a unit, where
   * a write is its own unit. A check that throws rolls the whole unit back. This is a deferred constraint: what a
   * unit leaves inconsistent part way is allowed, what it commits is not.
   */
  atCommit(check: () => void): void;
}

/** A journal over the store, and the sink that holds an open unit's calls back from `sink` */
export function createJournal(sink: PersistenceSink, access: StoreAccess) {
  const levels: Level[] = [];
  const held: Array<() => void> = [];

  const journal: Journal = {
    get open() { return levels.length > 0; },
    atCommit(check) {
      const level = levels[levels.length - 1];
      if (level) level.checks.push(check);
      else check();
    },
  };

  // A held call copies an array it carries: the caller who wrote it may still change it before the unit commits
  const hold = <A extends unknown[]>(write: (...args: A) => void) => (...args: A): void => {
    if (levels.length === 0) return write(...args);
    const copied = args.map((arg) => (Array.isArray(arg) ? [...arg] : arg)) as A;
    held.push(() => write(...copied));
  };

  /** `sink`, except that its writes wait for the outermost unit to commit */
  const unitSink: PersistenceSink = {
    onCreateEntity: hold((...args) => sink.onCreateEntity(...args)),
    onDestroyEntity: hold((...args) => sink.onDestroyEntity(...args)),
    onDropAttr: hold((...args) => sink.onDropAttr(...args)),
    onPutAttr: hold((...args) => sink.onPutAttr(...args)),
    onAddRelation: hold((...args) => sink.onAddRelation(...args)),
    onUpdateRelation: hold((...args) => sink.onUpdateRelation(...args)),
    onRemoveRelation: hold((...args) => sink.onRemoveRelation(...args)),
    onUnitStart: () => sink.onUnitStart(),
    onUnitEnd: () => sink.onUnitEnd(),
    close: sink.close && (() => sink.close?.()),
    getErrorStats: sink.getErrorStats && (() => sink.getErrorStats!()),
  };

  /** Runs `fn` as one unit; a unit inside another commits into it and rolls back only its own writes */
  function transact<T>(fn: () => T): T {
    const level: Level = { sinkMark: held.length, checks: [] };
    levels.push(level);
    let result: T;
    try {
      result = access.unit(() => {
        const value = fn();
        if (value !== null && typeof value === 'object' && typeof (value as { then?: unknown }).then === 'function') {
          // The body has already started and cannot be stopped; what it does after its first await lands outside
          // any unit. Its own rejection is handled here so it isn't an unhandled one: the refusal is the report
          (value as unknown as PromiseLike<unknown>).then(undefined, () => {});
          throw new Error('transact() takes a synchronous function: an await inside it would split the unit');
        }
        // The outermost unit is what commits, so it is where the unit's checks run; an inner one hands them up
        if (levels.length === 1) for (const check of level.checks) check();
        return value;
      });
    } catch (error) {
      levels.pop();
      held.length = level.sinkMark;
      throw error;
    }
    levels.pop();
    const parent = levels[levels.length - 1];
    if (parent) {
      parent.checks.push(...level.checks);
      return result;
    }
    const calls = held.splice(0);
    if (calls.length) {
      sink.onUnitStart();
      try {
        for (const call of calls) call();
      } finally {
        sink.onUnitEnd();
      }
    }
    return result;
  }

  return { journal, unitSink, transact };
}
