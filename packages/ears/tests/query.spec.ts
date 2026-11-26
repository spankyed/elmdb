import { afterEach, describe, expect, it } from 'vitest';
import { EarsError, createEarsEngine, isEarsError, type Schema } from '../src/index.ts';
import { createTemporaryDatabases } from '../src/lmdb/index.ts';

const querySchema: Schema = {
  types: {
    Doc: {
      fields: {
        title: { type: 'string' },
        body: { type: 'string' },
        category: { type: 'string' },
        score: { type: 'integer' },
        publishedAt: { type: 'integer', optional: true },
        note: { type: 'string', optional: true, nullable: true },
      },
      indexes: {
        byCategory: { kind: 'value', fields: ['category'] },
        byTitle: { kind: 'order', field: 'title', locale: 'en' },
        byScore: { kind: 'value', fields: ['score'] },
        searchIdx: { kind: 'text', fields: [['title', 2.0], ['body', 1.0]] },
        titleTrigram: { kind: 'trigram', fields: ['title'] },
        vecIdx: { kind: 'vector', fields: ['title'], dims: 3 },
      },
    },
  },
  relations: {
    cites: {
      cardinality: 'manyToMany',
      endpoints: [{ source: 'Doc', target: 'Doc' }],
      weighted: true,
    },
  },
};

const temps = createTemporaryDatabases();

afterEach(() => {
  temps.disposeAll();
});

describe('@elmdb/ears algebraic QueryBuilder', () => {
  it('supports filters, ICU ordering, keyset pagination, aggregates, async terminals, and immutable branching', async () => {
    const { database } = temps.open({ schema: querySchema });
    const engine = createEarsEngine({ database });

    engine.batch((b) => {
      b.create('Doc', { title: 'Alpha Guide', body: 'rust lmdb storage', category: 'eng', score: 10, publishedAt: 1000, note: 'pinned' }, 'd1');
      b.create('Doc', { title: 'beta notes', body: 'typescript query builder', category: 'eng', score: 30, publishedAt: 2000, note: null }, 'd2');
      b.create('Doc', { title: 'Gamma Draft', body: 'graph leiden pagerank', category: 'ops', score: 20, publishedAt: 3000 }, 'd3');
      b.create('Doc', { title: 'Delta Spec', body: 'vector hnsw search', category: 'eng', score: 40, publishedAt: 4000 }, 'd4');
      b.link('d1', 'cites', 'd2', { weight: 1.0 });
      b.link('d2', 'cites', 'd3', { weight: 1.0 });
      b.link('d3', 'cites', 'd1', { weight: 1.0 });
      b.link('d4', 'cites', 'd1', { weight: 1.0 });
    });

    // Base builder is immutable when branched
    const engBase = engine.query.of('Doc').where('category', 'eng');
    const highEng = engBase.where('score', { gte: 25 }).orderBy('score', 'desc');
    const lowEng = engBase.where('score', { lt: 25 });
    expect(highEng.ids()).toEqual(['d4', 'd2']);
    expect(lowEng.ids()).toEqual(['d1']);

    // Predicates: startsWith, contains, regex, in, ne, isNull, isPresent, whereNot, whereAny, hasLink
    expect(engine.query.of('Doc').where('title', { startsWith: 'Alpha' }).ids()).toEqual(['d1']);
    expect(engine.query.of('Doc').where('body', { contains: 'hnsw' }).ids()).toEqual(['d4']);
    expect(engine.query.of('Doc').where('title', { regex: '^Gamma' }).ids()).toEqual(['d3']);
    expect(engine.query.of('Doc').where('category', { in: ['ops'] }).ids()).toEqual(['d3']);
    expect(engine.query.of('Doc').where('category', { ne: 'eng' }).ids()).toEqual(['d3']);
    expect(engine.query.of('Doc').where('note', { isNull: true }).orderById('asc').ids()).toEqual(['d2', 'd3', 'd4']);
    expect(engine.query.of('Doc').where('note', { isPresent: true }).orderById('asc').ids()).toEqual(['d1']);
    expect(engine.query.of('Doc').hasLink({ relKinds: ['cites'], dir: 'out', targetId: 'd2' }).ids()).toEqual(['d1']);
    expect(
      engine.query
        .of('Doc')
        .whereAny([
          { op: 'eq', field: 'score', value: 10 },
          { op: 'eq', field: 'score', value: 40 },
        ])
        .orderById('asc')
        .ids(),
    ).toEqual(['d1', 'd4']);
    expect(engine.query.of('Doc').whereNot({ op: 'eq', field: 'category', value: 'eng' }).ids()).toEqual(['d3']);

    // Keyset pagination
    const page1 = engine.query.of('Doc').orderBy('score', 'asc').limit(2).all();
    expect(page1.map((r) => r.id)).toEqual(['d1', 'd3']);
    const page2 = engine.query
      .of('Doc')
      .orderBy('score', 'asc')
      .after({ id: 'd3', value: 20 })
      .limit(2)
      .all();
    expect(page2.map((r) => r.id)).toEqual(['d2', 'd4']);

    // Aggregates (sync & async)
    expect(engine.query.of('Doc').count()).toBe(4);
    expect(await engine.query.of('Doc').countAsync()).toBe(4);
    expect(engine.query.of('Doc').where('category', 'missing').exists()).toBe(false);
    expect(await engine.query.of('Doc').where('category', 'eng').existsAsync()).toBe(true);
    expect(engine.query.of('Doc').sum('score')).toBe(100);
    expect(await engine.query.of('Doc').sumAsync('score')).toBe(100);
    expect(engine.query.of('Doc').minMax('score')).toEqual({ min: 10, max: 40 });
    expect(await engine.query.of('Doc').minMaxAsync('score')).toEqual({ min: 10, max: 40 });
    expect(new Map(engine.query.of('Doc').groupCounts('category'))).toEqual(
      new Map([
        ['eng', 3],
        ['ops', 1],
      ]),
    );
    expect(engine.query.of('Doc').groupBy('category').length).toBe(2);
    expect(engine.query.of('Doc').degrees({ relKinds: ['cites'], dir: 'in' }).length).toBe(4);
    expect((await engine.query.of('Doc').orderBy('score', 'desc').firstAsync())?.id).toBe('d4');
    expect((await engine.query.of('Doc').orderBy('score', 'asc').idsAsync())[0]).toBe('d1');

    // Explain plan
    const explained = engine.query.of('Doc').where('category', 'eng').explain();
    expect(explained.explain).not.toBeNull();
  });

  it('supports BM25F, trigram fuzzy, vector near/like, score fusion, graph analytics, and explicit filterLocal', async () => {
    const { database } = temps.open({ schema: querySchema });
    const engine = createEarsEngine({ database });

    engine.batch((b) => {
      b.create('Doc', { title: 'Rust LMDB Engine', body: 'zero copy storage btree', category: 'eng', score: 10, publishedAt: 1000 }, 'd1');
      b.create('Doc', { title: 'Vector Search Guide', body: 'hnsw cosine embeddings', category: 'eng', score: 20, publishedAt: 2000 }, 'd2');
      b.create('Doc', { title: 'Graph Community Notes', body: 'leiden pagerank scc', category: 'ops', score: 30, publishedAt: 3000 }, 'd3');
      b.link('d1', 'cites', 'd2');
      b.link('d2', 'cites', 'd3');
      b.link('d3', 'cites', 'd1');
    });

    engine.setEmbeddings('vecIdx', 'd1', [[1, 0, 0]]);
    engine.setEmbeddings('vecIdx', 'd2', [[0, 1, 0]]);
    engine.setEmbeddings('vecIdx', 'd3', [[0, 0, 1]]);

    // BM25F full-text search
    const textHits = engine.query.of('Doc').search('searchIdx', 'rust storage', { topK: 5 }).hits();
    expect(textHits[0]?.id).toBe('d1');

    // Vector near + like + async hits
    const nearHits = await engine.query.of('Doc').near('vecIdx', [0, 0.99, 0.01], 2).hitsAsync();
    expect(nearHits[0]?.id).toBe('d2');
    const likeHits = engine.query.of('Doc').like('vecIdx', 'd1', 2).hits();
    expect(likeHits).toHaveLength(2);
    expect(likeHits.map((h) => h.id)).not.toContain('d1');

    // Fuzzy trigram + Levenshtein
    const fuzzyHits = engine.query.of('Doc').fuzzy('title', 'Vectro Search Guide', 2).hits();
    expect(fuzzyHits[0]?.id).toBe('d2');

    // Hybrid score fusion (RRF)
    const fused = engine.query
      .of('Doc')
      .fuse(
        [
          [{ kind: 'bm25f', index: 'searchIdx', query: 'hnsw', topK: 5 }, 1.0],
          [{ kind: 'vectorNear', index: 'vecIdx', vector: [0, 1, 0], topK: 5 }, 1.0],
        ],
        { method: 'rrf', rrfK: 60 },
      )
      .hits();
    expect(fused[0]?.id).toBe('d2');

    // Graph traversals & analytics
    expect(engine.query.ids(['d1']).hops({ relKinds: ['cites'], dir: 'out', minHops: 1, maxHops: 1 }).ids()).toEqual(['d2']);
    expect(engine.query.ids(['d1']).shortestPath('d3', { relKinds: ['cites'], dir: 'out', maxHops: 4 }).ids()).toEqual([
      'd1',
      'd2',
      'd3',
    ]);
    const ppr = engine.query.of('Doc').ppr([['d1', 1.0]], { relKinds: ['cites'] }).graphPpr();
    expect(ppr.scores.length).toBe(3);
    const scc = engine.query.of('Doc').scc(['cites']).graphGroups();
    expect(scc).toHaveLength(1);
    expect(scc[0]?.length).toBe(3);
    const comps = engine.query.of('Doc').weakComponents(['cites']).graphGroups();
    expect(comps).toHaveLength(1);
    const leiden = engine.query.of('Doc').leiden({ relKinds: ['cites'] }).graphGroups();
    expect(leiden.flat().length).toBe(3);
    const topo = engine.query.of('Doc').topoSort(['cites']).graphTopo();
    expect(topo.cycles).toBeGreaterThan(0);

    // Explicit filterLocal works on all()/first() and refuses aggregate/non-row terminals!
    const localRows = engine.query
      .of('Doc')
      .orderById('asc')
      .filterLocal((row) => String(row.fields['title']).includes('Vector'))
      .all();
    expect(localRows.map((r) => r.id)).toEqual(['d2']);

    let refused: unknown;
    try {
      engine.query.of('Doc').filterLocal(() => true).count();
    } catch (err) {
      refused = err;
    }
    expect(isEarsError(refused)).toBe(true);
    expect((refused as EarsError).code).toBe('INVALID_ARGUMENT');
  });
});
