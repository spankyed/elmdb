// What makes `transact` a unit: its writes run in one write unit of the store, which keeps nothing of them when it
// throws (`kv.ts`), and the checks it defers run as the outermost unit commits.
import type { UnitOptions } from '../database.ts';
import type { StoreAccess } from './store-access.ts';

/** One open unit: its checks */
interface Level {
  checks: Array<() => void>;
}

/** An engine's unit of work, as the writes see it */
export interface Journal {
  /** Whether a unit is open */
  readonly open: boolean;
  /**
   * Runs `check` when the outermost unit commits, before it does — at once outside a unit, where
   * a write is its own unit. A check that throws rolls the whole unit back. This is a deferred constraint: what a
   * unit leaves inconsistent part way is allowed, what it commits is not.
   */
  atCommit(check: () => void): void;
}

/** A journal over the store */
export function createJournal(access: StoreAccess) {
  const levels: Level[] = [];

  const journal: Journal = {
    get open() { return levels.length > 0; },
    atCommit(check) {
      const level = levels[levels.length - 1];
      if (level) level.checks.push(check);
      else check();
    },
  };

  /** Runs `fn` as one unit; a unit inside another commits into it and rolls back only its own writes */
  function transact<T>(fn: () => T, options?: UnitOptions): T {
    const level: Level = { checks: [] };
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
      }, options);
    } catch (error) {
      levels.pop();
      throw error;
    }
    levels.pop();
    const parent = levels[levels.length - 1];
    if (parent) {
      parent.checks.push(...level.checks);
      return result;
    }
    return result;
  }

  return { journal, transact };
}
