import type { LmdbDbs } from './envs.ts';
import type { PersistenceSink } from '../runtime.ts';

type Encoded = { t: string; v: any };

function enc(value: unknown): Encoded {
  if (value instanceof Date) return { t: 'date', v: value.toISOString() };
  const t = value === null ? 'null'
    : Array.isArray(value) ? 'array'
    : typeof value === 'object' ? 'object'
    : typeof value; // string|number|boolean|undefined
  return { t, v: value };
}

// Use same separator throughout for consistency
const SEP = '\x1F';

function attrKey(kind: string, entityId: string, idx: number) {
  validateKey(kind, 'kind');
  validateKey(entityId, 'entityId');
  return `${kind}${SEP}${entityId}${SEP}${idx}`;
}

function validateKey(value: string, name: string) {
  if (value.includes(SEP)) {
    throw new Error(`Invalid ${name}: contains forbidden separator character \\x1F`);
  }
}

function prefix(kind: string, entityId: string) {
  const p = `${kind}${SEP}${entityId}${SEP}`;
  // End bound: any key with this prefix sorts before prefix+\xFF
  return { start: p, end: p + '\xFF' };
}

function arrayRewriteKey(kind: string, entityId: string) {
  return `${kind}${SEP}${entityId}`;
}

/** The attribute kinds stored in `attrs`: keys sort by kind, so each lookup skips past the kind before it */
function* storedKinds(attrs: LmdbDbs['attrs']): Iterable<string> {
  let start = '';
  for (;;) {
    const [key] = attrs.getKeys({ start, limit: 1 }) as Iterable<string>;
    if (key === undefined) return;
    const kind = String(key).split(SEP)[0];
    yield kind;
    // Past every `kind␟…` key (ids sort below \xFF, as in prefix())
    start = `${kind}${SEP}\xFF`;
  }
}

/** What the app is told when a write is dropped. The engine reports; the app decides what to do about it. */
export interface WriteFailure {
  /** `entity` | `entity-remove` | `attrs` | `relation` | `relation-remove`, or `destroy` for a unit's destroy */
  op: string;
  /** The row the dropped write was for, which is what a diagnosis needs and the logs never had */
  key: string;
  error: unknown;
}

export function makeLmdbAdapter(dbs: LmdbDbs, onWriteFailure?: (failure: WriteFailure) => void): PersistenceSink {
  const { entities, attrs, relations } = dbs;

  // Error tracking
  let errorCount = 0;
  let lastError: { op: string; key?: string; error: any } | null = null;

  // Keyed buffers for coalescing writes
  const arrayRewrites = new Map<string, unknown[]>(); // kind\x1FentityId -> final array
  const relUpserts = new Map<string, any>();
  const relDeletes = new Set<string>();
  // An entity's row holds its type, written once, when the entity is created
  const createdRows = new Map<string, string>();
  
  let scheduled = false;
  let closed = false;
  // A committed unit (`onUnitStart` to `onUnitEnd`): its destroys wait with its other writes, in order
  let inUnit = false;
  const unitDestroys: string[] = [];

  /**
   * The pending writes as independent steps, each naming the row it touches, with the buffers taken so new
   * writes accumulate behind it.
   *
   * It is a list rather than one body because `flushBody` used to run every write in a single
   * `transactionSync`: one bad `put` aborted the whole transaction, and the buffers were then cleared, so
   * every *unrelated* write in that microtask was lost with it. That is how a user's note died as collateral
   * damage for something else's failure. Keeping them separable costs nothing on the happy path — they still
   * go in one transaction — and on failure it lets each be retried alone.
   */
  function takeSteps(): Array<{ op: string; key: string; run: () => void }> {
    const steps: Array<{ op: string; key: string; run: () => void }> = [];

    for (const [id, type] of createdRows) steps.push({ op: 'entity', key: id, run: () => { entities.put(id, { type }); } });
    createdRows.clear();

    for (const [key, arr] of arrayRewrites) {
      const [kind, entityId] = key.split(SEP);
      steps.push({ op: 'attrs', key, run: () => {
        removeAttrRange(kind, entityId);
        for (let i = 0; i < arr.length; i++) attrs.put(attrKey(kind, entityId, i), enc(arr[i]));
      } });
    }
    arrayRewrites.clear();

    for (const id of relDeletes) steps.push({ op: 'relation-remove', key: id, run: () => { relations.remove(id); } });
    relDeletes.clear();

    for (const [id, obj] of relUpserts) steps.push({ op: 'relation', key: id, run: () => { relations.put(id, obj); } });
    relUpserts.clear();

    return steps;
  }

  /**
   * Every attribute row of one `(kind, entityId)`.
   *
   * The copy is the point: the body removes exactly the keys this walks, and an lmdb range is a live cursor.
   * `onDestroyEntity` said so in a comment of its own; saying it twice in one file is how the two drifted.
   */
  function removeAttrRange(kind: string, entityId: string): void {
    // The spread is the whole point, so the lint rule calling it useless is wrong here: an lmdb range is a
    // live cursor, and the body removes exactly the keys it walks
    // eslint-disable-next-line no-useless-spread
    for (const key of [...attrs.getKeys(prefix(kind, entityId))]) attrs.remove(key);
  }

  /** Runs the steps in one transaction, falling back to one transaction each so a failure is isolated. */
  function runSteps(steps: Array<{ op: string; key: string; run: () => void }>, op: string): void {
    if (steps.length === 0) return;
    try {
      entities.transactionSync(() => { for (const step of steps) step.run(); });
      return;
    } catch {
      // Fall through: the transaction is rolled back whole, so every step is still unapplied
    }
    for (const step of steps) {
      try {
        entities.transactionSync(step.run);
      } catch (error) {
        errorCount++;
        lastError = { op, key: step.key, error };
        onWriteFailure?.({ op: step.op, key: step.key, error });
      }
    }
  }

  /** Runs a unit's steps in one transaction: a unit is stored whole or not at all, so a failure has no retry */
  function runUnit(steps: Array<{ op: string; key: string; run: () => void }>): void {
    if (steps.length === 0) return;
    try {
      entities.transactionSync(() => { for (const step of steps) step.run(); });
    } catch (error) {
      for (const step of steps) {
        errorCount++;
        lastError = { op: step.op, key: step.key, error };
        onWriteFailure?.({ op: step.op, key: step.key, error });
      }
    }
  }

  /** An entity's row and its attributes, read per kind (keys are kind␟id␟index); its relations went before */
  function removeEntityRows(entityId: string): void {
    entities.remove(entityId);
    // `storedKinds` reads the same store this loop removes from, inside one transaction, so it is materialised
    // for the same reason `removeAttrRange` materialises: a live cursor the body invalidates
    // eslint-disable-next-line no-useless-spread
    for (const kind of [...storedKinds(attrs)]) removeAttrRange(kind, entityId);
  }

  function scheduleFlush() {
    if (scheduled || closed) return;
    scheduled = true;
    queueMicrotask(() => {
      if (closed) return;
      scheduled = false;
      
      runSteps(takeSteps(), 'flush');
    });
  }

  function bufferArrayRewrite(kind: string, entityId: string, array: unknown[]) {
    // Validate keys early to fail fast
    validateKey(kind, 'kind');
    validateKey(entityId, 'entityId');
    
    const key = arrayRewriteKey(kind, entityId);
    // Store only the final state of the array
    arrayRewrites.set(key, [...array]); // Clone to avoid mutations
    scheduleFlush();
  }

  /** Drops the writes buffered for an entity, so a flush after its hard delete doesn't write it back */
  function discardBuffered(entityId: string) {
    createdRows.delete(entityId);
    for (const key of arrayRewrites.keys()) {
      if (key.slice(key.indexOf(SEP) + 1) === entityId) arrayRewrites.delete(key);
    }
  }

  const buffered = () =>
    createdRows.size + arrayRewrites.size + relDeletes.size + relUpserts.size > 0;

  function close() {
    if (closed) return;
    closed = true;
    // Nothing to write (a read-only environment can't open a write transaction)
    if (!buffered()) return;

    // Flush whatever is in the buffers, regardless of scheduled state. `takeSteps` empties them, and
    // `runSteps` isolates a failure to the one row it belongs to, as the scheduled flush does.
    runSteps(takeSteps(), 'final flush');
  }

  /**
   * A relation write this adapter refuses: counted like any other failed write, so `close()` reports it instead of
   * the write disappearing with a line in the console
   */
  function dropRelation(relId: string, reason: string): void {
    errorCount++;
    lastError = { op: 'relation', key: relId, error: new Error(reason) };
    console.warn(`[LMDB] ${reason}`);
  }

  return {
    onCreateEntity(entityId: string, type: string) {
      if (closed) return;
      createdRows.set(entityId, type);
      scheduleFlush();
    },

    onDestroyEntity(entityId: string) {
      if (closed) return;

      discardBuffered(entityId);
      if (inUnit) {
        unitDestroys.push(entityId);
        return;
      }
      // Deleted now, outside a unit. Its relations were removed before this, each with onRemoveRelation.
      try {
        entities.transactionSync(() => removeEntityRows(entityId));
      } catch (error) {
        errorCount++;
        lastError = { op: 'destroy', key: entityId, error };
        console.error(`[LMDB] Failed to delete entity ${entityId}:`, error);
      }
    },

    onUnitStart() {
      if (closed) return;
      // What was buffered before the unit is not part of it, and keeps its own row-by-row isolation
      runSteps(takeSteps(), 'flush');
      inUnit = true;
    },

    onUnitEnd() {
      inUnit = false;
      if (closed) return;
      // Destroys first: a write after a destroy in the unit (an id created again) lands after it
      const destroys = unitDestroys.splice(0).map((id) => ({ op: 'destroy', key: id, run: () => removeEntityRows(id) }));
      runUnit([...destroys, ...takeSteps()]);
    },

    onPutAttrArray(kind: string, entityId: string, values: unknown[]) {
      if (closed) return;
      bufferArrayRewrite(kind, entityId, values);
    },

    onDropAttr(kind: string, entityId: string, idx: number, entireArray?: unknown[]) {
      if (closed) return;
      // Always rewrite the entire array after drop
      if (entireArray !== undefined) {
        bufferArrayRewrite(kind, entityId, entireArray);
      } else {
        // Fallback: delete everything for this (kind, entityId)
        bufferArrayRewrite(kind, entityId, []);
      }
    },

    onAddRelation(relId: string, kind: string, src: string, tgt: string, info: unknown) {
      if (closed) return;
      
      // Validate src and tgt are valid entity IDs (not stringified undefined/null)
      if (!src || src === 'undefined' || src === 'null' || typeof src !== 'string') {
        dropRelation(relId, `Invalid src in onAddRelation: relId=${relId}, src="${src}"`);
        return;
      }
      if (!tgt || tgt === 'undefined' || tgt === 'null' || typeof tgt !== 'string') {
        dropRelation(relId, `Invalid tgt in onAddRelation: relId=${relId}, tgt="${tgt}"`);
        return;
      }
      // Basic entity ID format check (should contain hyphen)
      if (!src.includes('-')) {
        dropRelation(relId, `Invalid src format in onAddRelation: relId=${relId}, src="${src}" (missing hyphen)`);
        return;
      }
      if (!tgt.includes('-')) {
        dropRelation(relId, `Invalid tgt format in onAddRelation: relId=${relId}, tgt="${tgt}" (missing hyphen)`);
        return;
      }
      
      // A relation written again (routed here after a reopen) keeps when it was created
      const createdAt = relDeletes.has(relId) ? undefined : (relUpserts.get(relId) ?? relations.get(relId))?.createdAt;
      relDeletes.delete(relId); // Cancel any pending delete
      relUpserts.set(relId, { kind, src, tgt, info: info ?? null, createdAt: createdAt ?? Date.now() });
      scheduleFlush();
    },

    onUpdateRelation(relId: string, patch: { src?: string; tgt?: string; info?: unknown }) {
      if (closed) return;
      const r = relations.get(relId) || relUpserts.get(relId);
      if (!r) {
        dropRelation(relId, `onUpdateRelation called on missing relId: ${relId}`);
        return;
      }
      
      const updated = { ...r };
      if (patch.src) {
        // Validate src is a valid entity ID
        if (patch.src === 'undefined' || patch.src === 'null' || typeof patch.src !== 'string') {
          dropRelation(relId, `Invalid src in onUpdateRelation: relId=${relId}, src="${patch.src}"`);
          return;
        }
        if (!patch.src.includes('-')) {
          dropRelation(relId, `Invalid src format in onUpdateRelation: relId=${relId}, src="${patch.src}" (missing hyphen)`);
          return;
        }
        updated.src = patch.src;
      }
      if (patch.tgt) {
        // Validate tgt is a valid entity ID
        if (patch.tgt === 'undefined' || patch.tgt === 'null' || typeof patch.tgt !== 'string') {
          dropRelation(relId, `Invalid tgt in onUpdateRelation: relId=${relId}, tgt="${patch.tgt}"`);
          return;
        }
        if (!patch.tgt.includes('-')) {
          dropRelation(relId, `Invalid tgt format in onUpdateRelation: relId=${relId}, tgt="${patch.tgt}" (missing hyphen)`);
          return;
        }
        updated.tgt = patch.tgt;
      }
      if ('info' in patch) {
        updated.info = patch.info ?? null;
      }
      
      relUpserts.set(relId, updated);
      scheduleFlush();
    },

    onRemoveRelation(relId: string) {
      if (closed) return;
      relUpserts.delete(relId); // Cancel any pending upsert
      relDeletes.add(relId);
      scheduleFlush();
    },

    close,
    
    getErrorStats() {
      return { errorCount, lastError };
    }
  };
}