// What makes `transact` a transaction: while a unit is open, every write first records how to undo itself, and
// the sink calls it makes are held rather than sent. A unit that returns sends them; one that throws runs the
// undos in reverse and sends nothing.
import type { PersistenceSink } from './runtime.ts';

/** One open unit: the keys it has recorded an undo for, the undos, and where its sink calls start */
interface Level {
  seen: Set<string>;
  undos: Array<() => void>;
  sinkMark: number;
}

/** An engine's unit of work, shared by the stores that write */
export interface Journal {
  /** Whether a unit is open: a writer checks it before building a key, so writes outside a unit cost nothing */
  readonly open: boolean;
  /**
   * Records how to undo a change about to be made to `key`. `capture` runs only inside a unit and only on the
   * first change to that key in the innermost unit, before the change, and returns the undo.
   */
  touch(key: string, capture: () => () => void): void;
}

/** A journal, and the sink that holds an open unit's calls back from `sink` */
export function createJournal(sink: PersistenceSink) {
  const levels: Level[] = [];
  const held: Array<() => void> = [];

  const journal: Journal = {
    get open() { return levels.length > 0; },
    touch(key, capture) {
      const level = levels[levels.length - 1];
      if (!level || level.seen.has(key)) return;
      level.seen.add(key);
      level.undos.push(capture());
    },
  };

  // A held call copies its value lists: the storage passes its live list, which a later write in the unit — or
  // a nested unit that then rolls back — would otherwise change before the call is sent
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
    onPutAttrArray: hold((...args) => sink.onPutAttrArray(...args)),
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
    const level: Level = { seen: new Set(), undos: [], sinkMark: held.length };
    levels.push(level);
    let result: T;
    try {
      result = fn();
      if (result !== null && typeof result === 'object' && typeof (result as { then?: unknown }).then === 'function') {
        // The body has already started and cannot be stopped; what it does after its first await lands outside
        // any unit. Its own rejection is handled here so it isn't an unhandled one: the refusal is the report
        (result as unknown as PromiseLike<unknown>).then(undefined, () => {});
        throw new Error('transact() takes a synchronous function: an await inside it would split the unit');
      }
    } catch (error) {
      levels.pop();
      for (let i = level.undos.length - 1; i >= 0; i--) level.undos[i]();
      held.length = level.sinkMark;
      throw error;
    }
    levels.pop();
    const parent = levels[levels.length - 1];
    if (parent) {
      // The parent can still roll back, to before this unit and before its own earlier writes: this unit's
      // undos restore what it found, and run before the parent's earlier ones
      parent.undos.push(...level.undos);
      for (const key of level.seen) parent.seen.add(key);
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
