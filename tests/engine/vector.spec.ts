// Vector search through the builder: a type's `embed` declaration as elmdb's vector index, chunks pending until
// `embed` stores their vectors, `near`, `like` and `hybrid` ranked and combined with the steps around them. How elmdb
// chunks, files and ranks is its own specs' (elmdb/crates/elmdb/src/engine/vector/tests.rs); these hold the builder
// and the engine's members to sending what they mean.
import { describe, expect, it } from 'vitest';
import { EarsError, type EarsEngine } from '../../src/index.ts';
import { freshEngine } from './engine-under-test.ts';

const engine = () => freshEngine({
  types: ['Note', 'Plain'],
  relationKinds: ['in'],
  roles: [],
  rowRules: {
    Note: { required: [], search: { fields: { title: 1 } }, embed: { from: ['title', 'body'], model: 'openai:test-embedding', dims: 3 } },
  },
});

/** A fixed corpus's vectors: each text's meaning, as an axis of three */
const MEANINGS: Record<string, readonly number[]> = {
  apples: [1, 0, 0],
  pears: [0.9, 0.1, 0],
  boats: [0, 1, 0],
  ships: [0.1, 0.9, 0],
  stars: [0, 0, 1],
};
const vectorOf = (text: string) => MEANINGS[text.split('\n\n').at(-1)!] ?? [1, 1, 1];

/** Embeds every pending chunk of Note with its corpus vector */
function embedAll(e: EarsEngine) {
  const { model, chunks } = e.pendingEmbeddings('Note');
  return e.embed(chunks.map((chunk) => ({ ...chunk, model, vector: vectorOf(chunk.text) })));
}

describe('vector search', () => {
  it('holds a written entity pending until its vectors are stored, and again when its text changes', () => {
    const e = engine();
    const a = e.create('Note', { title: 'fruit', body: 'apples' }).id();
    const pending = e.pendingEmbeddings('Note');
    expect(pending).toEqual({ model: 'openai:test-embedding', dims: 3, count: 1, chunks: [{ id: a, chunk: 0, hash: expect.any(String), text: 'fruit\n\napples' }] });
    expect(e.query('Note').near([1, 0, 0]).ids()).toEqual([]);

    expect(embedAll(e)).toEqual({ stored: 1, stale: 0 });
    expect(e.pendingEmbeddings('Note').count).toBe(0);
    expect(e.query('Note').near([1, 0, 0]).ids()).toEqual([a]);

    e.write(a).set('body', 'boats');
    expect(e.pendingEmbeddings('Note').chunks.map(({ text }) => text)).toEqual(['fruit\n\nboats']);
    expect(e.query('Note').near([1, 0, 0]).ids()).toEqual([]);
    // A field the index doesn't read changes nothing
    embedAll(e);
    e.write(a).set('color', 'red');
    expect(e.pendingEmbeddings('Note').count).toBe(0);
  });

  it('refuses a vector whose chunk changed since it was pending, leaving the chunk pending under its new text', () => {
    const e = engine();
    const a = e.create('Note', { title: 'x', body: 'apples' }).id();
    const { model, chunks } = e.pendingEmbeddings('Note');
    e.write(a).set('body', 'pears');
    expect(e.embed(chunks.map((chunk) => ({ ...chunk, model, vector: [1, 0, 0] })))).toEqual({ stored: 0, stale: 1 });
    expect(e.pendingEmbeddings('Note').chunks.map(({ text }) => text)).toEqual(['x\n\npears']);
  });

  it('counts a vector for an entity deleted since it was pending as stale, and stores the rest of the batch', () => {
    const e = engine();
    const gone = e.create('Note', { title: 'x', body: 'pears' }).id();
    e.create('Note', { title: 'y', body: 'boats' });
    const { model, chunks } = e.pendingEmbeddings('Note');
    e.write(gone).destroy();
    expect(e.embed(chunks.map((chunk) => ({ ...chunk, model, vector: vectorOf(chunk.text) })))).toEqual({ stored: 1, stale: 1 });
    expect(e.pendingEmbeddings('Note').count).toBe(0);
  });

  it('ranks the nearest first, combines with the steps before it, and finds more like an entity', () => {
    const e = engine();
    const [apples, pears, boats, ships] = ['apples', 'pears', 'boats', 'ships'].map((body) => e.create('Note', { title: 'a note', body }).id());
    embedAll(e);
    expect(e.query('Note').near([1, 0, 0], { k: 2 }).ids()).toEqual([apples, pears]);
    expect(e.query('Note').near([0, 1, 0]).limit(1).ids()).toEqual([boats]);
    expect(e.query.ids([pears!, ships!]).near([1, 0, 0]).ids()).toEqual([pears, ships]);
    expect(e.query('Note').like(boats!, { k: 1 }).ids()).toEqual([ships]);
    const [hit] = e.query('Note').near([1, 0, 0], { k: 1 }).hits();
    expect(hit).toEqual({ id: apples, score: expect.any(Number), terms: [] });
    expect(hit!.score).toBeCloseTo(1, 5);
  });

  it('merges the text and vector rankings on a fixed corpus: what both find ranks above what one finds first', () => {
    const e = engine();
    const [ships, pears, stars, boats] = [['stars', 'ships'], ['stars', 'pears'], ['stars', 'stars'], ['boats', 'boats']]
      .map(([title, body]) => e.create('Note', { title, body }).id());
    embedAll(e);
    // Every note is in the vector ranking, the boat's first; the text ranking holds the three titled "stars"
    expect(e.query('Note').near([0, 1, 0]).ids()).toEqual([boats, ships, pears, stars]);
    expect(new Set(e.query('Note').search('stars').ids())).toEqual(new Set([ships, pears, stars]));
    const merged = e.query('Note').hybrid('stars', [0, 1, 0], { k: 4 }).ids();
    expect(merged).toHaveLength(4);
    expect(new Set(merged.slice(0, 3))).toEqual(new Set([ships, pears, stars]));
    expect(merged[3]).toBe(boats);
    expect(e.query('Note').hybrid('stars', [0, 1, 0], { k: 3 }).ids()).not.toContain(boats);
  });

  it('refuses the vector steps on a type with no vector index, and names the types holding one', () => {
    const e = engine();
    expect(e.embeddedTypes()).toEqual(['Note']);
    expect(() => e.query('Plain').near([1, 0, 0]).ids()).toThrow(expect.objectContaining({ code: 'UNKNOWN_INDEX' }));
    expect(() => e.pendingEmbeddings('Plain')).toThrow(EarsError);
    expect(() => e.query('Note').limit(2).hits()).toThrow(expect.objectContaining({ code: 'NO_SEARCH' }));
  });
});
