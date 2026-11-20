// A reader's checkpoint: set in a unit with the work it records, read back as the latest commit left it, and gone
// with the rows it describes
import { beforeEach, expect, it } from 'vitest';
import { engine } from './helpers.ts';
import type { EngineUnderTest } from './engine-under-test.ts';

let e: EngineUnderTest;
beforeEach(() => { e = engine(); });

it('commits with the unit that set it, and not with one that threw', () => {
  expect(e.admin.checkpoint('reader')).toBeUndefined();
  e.transact(() => {
    e.create('Task', { title: 'read' });
    e.admin.setCheckpoint('reader', { position: { sequence: 3 }, version: 1 });
    expect(e.admin.checkpoint('reader'), 'the unit sees its own').toEqual({ position: { sequence: 3 }, version: 1 });
  });
  expect(e.admin.checkpoint('reader')).toEqual({ position: { sequence: 3 }, version: 1 });

  expect(() => e.transact(() => {
    e.admin.setCheckpoint('reader', { position: { sequence: 9 }, version: 1 });
    throw new Error('the work failed');
  })).toThrow('the work failed');
  expect(e.admin.checkpoint('reader'), 'undone with its unit').toEqual({ position: { sequence: 3 }, version: 1 });
});

it('commits a unit that set only a checkpoint', () => {
  e.transact(() => { e.admin.setCheckpoint('reader', 'alone'); });
  expect(e.admin.checkpoint('reader')).toBe('alone');
});

it('is set only inside a unit, with the work it records', () => {
  expect(() => e.admin.setCheckpoint('reader', 'loose')).toThrow('inside a unit');
});

it('goes with the rows when the database is cleared', () => {
  e.transact(() => { e.admin.setCheckpoint('reader', 'there'); });
  e.admin.clear();
  expect(e.admin.checkpoint('reader')).toBeUndefined();
});
