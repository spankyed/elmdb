import { afterAll, bench, describe } from 'vitest';
import { createEarsEngine, type Schema } from '../src/index.ts';
import { createTemporaryDatabases } from '../src/lmdb/index.ts';

const benchSchema: Schema = {
  types: {
    Item: {
      fields: {
        title: { type: 'string' },
        bucket: { type: 'integer' },
      },
      indexes: {
        byBucket: { kind: 'value', fields: ['bucket'] },
        byTitle: { kind: 'text', fields: [['title', 1.0]] },
      },
    },
  },
  relations: {},
};

const temps = createTemporaryDatabases();
const { database } = temps.open({ schema: benchSchema, durability: 'unsafe', buildPolicy: 'eager' });
const engine = createEarsEngine({ database });

engine.batch((b) => {
  for (let i = 0; i < 500; i++) {
    b.create('Item', { title: `Benchmark item ${i} rust lmdb`, bucket: i % 10 }, `it-${i}`);
  }
});

afterAll(() => {
  temps.disposeAll();
});

describe('@elmdb/ears v2 microbenchmarks', () => {
  bench('point get by ID', () => {
    engine.get('it-42');
  });

  bench('indexed equality query (ids)', () => {
    engine.query.of('Item').where('bucket', 3).ids();
  });

  bench('BM25F full-text search (top 10)', () => {
    engine.query.of('Item').search('byTitle', 'rust lmdb', { topK: 10 }).hits();
  });
});
