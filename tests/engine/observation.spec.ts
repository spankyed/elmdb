import { describe, expect, it } from 'vitest';
import { engine } from './helpers.ts';
import type { EarsOperation, EarsOperationResult } from '../../src/index.ts';

describe('operation observation', () => {
  it('observes encoded byte counts and stops without changing call counting', () => {
    const e = engine();
    const operations: EarsOperation[] = [];
    const results: EarsOperationResult[] = [];
    const stop = e.admin.observe({ start(op) { operations.push(op); return result => { results.push(result); }; } });
    e.query('Task').ids();
    expect(operations).toMatchObject([{ kind: 'read', ops: 1, watch: false, inUnit: false }]);
    expect(results[0]).toMatchObject({ outcome: 'ok' });
    expect(results[0]!.requestBytes).toBeGreaterThan(0);
    expect(results[0]!.responseBytes).toBeGreaterThan(0);
    expect(e.admin.calls.report().counting).toBe(false);
    stop(); stop();
    e.query('Task').ids();
    expect(results).toHaveLength(1);
  });

  it('cannot replace a successful result or the original callback error', () => {
    const e = engine();
    const stop = e.admin.observe({ start() { throw new Error('recorder failed'); } });
    expect(e.query('Task').ids()).toEqual([]);
    stop();
    e.admin.observe({ start() { return () => { throw new Error('export failed'); }; } });
    const original = new Error('application failed');
    expect(() => e.transact(() => { throw original; })).toThrow(original);
    expect(e.query('Task').ids()).toEqual([]);
  });

  it('keeps nested units and writes separate and reports rollback', () => {
    const e = engine();
    const events: Array<{ operation: EarsOperation; result: EarsOperationResult }> = [];
    e.admin.observe({ start(operation) { return result => { events.push({ operation, result }); }; } });
    expect(() => e.transact(() => {
      e.create('Task', { title: 'rolled back' });
      e.transact(() => e.query('Task').ids());
      throw new Error('rollback');
    })).toThrow('rollback');
    expect(events.filter(event => event.operation.kind === 'unit').map(event => event.result.outcome)).toEqual(['ok', 'error']);
    expect(events.find(event => event.operation.kind === 'write')!.operation.inUnit).toBe(true);
    expect(e.query('Task').ids()).toEqual([]);
  });

  it('reports a committed durability failure without disguising it as a refused write', () => {
    const e = engine({ wrap: database => new Proxy(database, {
      get(target, key) {
        if (key === 'write') return (batch: Uint8Array, durable?: boolean) => {
          target.write(batch, durable);
          throw Object.assign(new Error('sync failed'), { code: 'NOT_DURABLE' });
        };
        const value = Reflect.get(target, key, target);
        return typeof value === 'function' ? value.bind(target) : value;
      },
    }) });
    const results: EarsOperationResult[] = [];
    e.admin.observe({ start() { return result => { results.push(result); }; } });
    expect(() => e.create('Task', { title: 'committed' })).toThrow();
    expect(results.some(result => result.outcome === 'committed-not-durable' && (result.sequence ?? 0) > 0 && result.errorCode === 'NOT_DURABLE')).toBe(true);
    expect(e.query('Task').ids()).toHaveLength(1);
  });

  it('does not fail an operation because diagnostic sequence lookup fails', () => {
    let refuseSequence = false;
    const e = engine({ wrap: database => new Proxy(database, {
      get(target, key) {
        if (key === 'sequence' && refuseSequence) throw new Error('metadata unavailable');
        const value = Reflect.get(target, key, target);
        return typeof value === 'function' ? value.bind(target) : value;
      },
    }) });
    const results: EarsOperationResult[] = [];
    e.admin.observe({ start() { return result => { results.push(result); }; } });
    refuseSequence = true;
    expect(e.query('Task').ids()).toEqual([]);
    expect(results[0]).toMatchObject({ outcome: 'ok', sequence: undefined });
  });

  it('does not recursively observe recorder reads', () => {
    const e = engine();
    let starts = 0;
    let finishes = 0;
    e.admin.observe({ start() {
      starts++;
      e.query('Task').ids();
      return () => { finishes++; e.query('Task').ids(); };
    } });
    expect(e.query('Task').ids()).toEqual([]);
    expect({ starts, finishes }).toEqual({ starts: 1, finishes: 1 });
  });
});
