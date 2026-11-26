// Packed-consumer TypeScript smoke test: packs `crates/elmdb-node` into an isolated directory with no `@types/node`
// and checks `elmdb`, `elmdb/client`, and `elmdb/protocol/*` under strict consumer compiler settings across
// `nodenext`, `node16`, and `bundler` module resolutions.
import { test, before, after } from 'node:test';
import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { mkdirSync, mkdtempSync, readdirSync, rmSync, writeFileSync } from 'node:fs';
import { createRequire } from 'node:module';
import { tmpdir } from 'node:os';
import { basename, dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const require = createRequire(import.meta.url);
const pkgDir = dirname(dirname(fileURLToPath(import.meta.url)));
const tscBin = require.resolve('typescript/bin/tsc');

const RESOLUTIONS = [
  { name: 'nodenext', module: 'nodenext', moduleResolution: 'nodenext' },
  { name: 'node16', module: 'node16', moduleResolution: 'node16' },
  { name: 'bundler', module: 'esnext', moduleResolution: 'bundler' },
];

let consumerDir;
let protocolNames;

before(() => {
  consumerDir = mkdtempSync(join(tmpdir(), 'elmdb-consumer-'));
  const packedJson = execFileSync(
    'npm',
    ['pack', pkgDir, '--pack-destination', consumerDir, '--json', '--ignore-scripts'],
    {
      cwd: consumerDir,
      encoding: 'utf8',
      shell: process.platform === 'win32',
      env: { ...process.env, npm_config_cache: join(consumerDir, '.npm-cache') },
    },
  );
  const [{ filename }] = JSON.parse(packedJson.slice(packedJson.indexOf('[')));
  const installed = join(consumerDir, 'node_modules', 'elmdb');
  mkdirSync(installed, { recursive: true });
  execFileSync('tar', ['-xzf', join(consumerDir, basename(filename)), '-C', installed, '--strip-components', '1'], {
    cwd: consumerDir,
    encoding: 'utf8',
  });
  protocolNames = readdirSync(join(installed, 'protocol'))
    .filter((file) => file.endsWith('.d.ts'))
    .map((file) => file.slice(0, -'.d.ts'.length))
    .sort();
  assert.ok(protocolNames.length > 0, 'packed package holds protocol declarations');
});

after(() => {
  if (consumerDir) rmSync(consumerDir, { recursive: true, force: true });
});

function consumerSource() {
  const protocolExports = protocolNames
    .flatMap((name, i) => [
      `export type * as ProtoJs${i} from 'elmdb/protocol/${name}.js';`,
      `export type * as ProtoBare${i} from 'elmdb/protocol/${name}';`,
    ])
    .join('\n');

  return `
import {
  Database,
  type BatchResult,
  type CheckReport,
  type DatabaseStats,
  type DumpReport,
  type ErrorCode,
  type IndexStatus,
  type OpenOptions,
  type OpenReport,
  type QueryAnswer,
  type QuerySpec,
  type RelationRow,
  type Row,
  type Schema,
  type SchemaChange,
  type WatchTick,
} from 'elmdb';
import { BatchSlot, WireBatchBuilder, buildWireBatch } from 'elmdb/client';
import type { WireBatchBuilder as WireBatchBuilderFromJs } from 'elmdb/client.js';
import type { ErrorCode as ProtocolErrorCode } from 'elmdb/protocol/ErrorCode';
${protocolExports}

export async function exerciseBinding(dir: string, schema: Schema, spec: QuerySpec): Promise<void> {
  const options: OpenOptions = { readOnly: false, schema, durability: 'strict', buildPolicy: 'deferred' };
  const db: Database = Database.open(dir, options);
  const dbAsync: Database = await Database.openAsync(dir, options);
  void dbAsync;

  const builder: WireBatchBuilder = new WireBatchBuilder();
  const slot: BatchSlot = builder.create('Note', { title: 'hi' });
  const frame: Uint8Array = buildWireBatch((b) => {
    const s = b.create('Note', { title: 'v2' });
    b.patch(s, { title: 'v2!' });
  });
  void slot;
  const builderAlias: WireBatchBuilderFromJs = builder;
  void builderAlias;

  const batchRes: BatchResult = db.executeWireBatch(frame);
  const report: OpenReport = db.openReport();
  const change: SchemaChange = db.setSchema(schema);
  const current: Schema = db.getSchema();
  const row: Row | null = db.get('id');
  const edges: RelationRow[] = db.getEdges('id');
  const answer: QueryAnswer = db.query(spec);
  const asyncAnswer: QueryAnswer = await db.queryAsync(spec);
  const checked: CheckReport = db.check();
  const checkedAsync: CheckReport = await db.checkAsync();
  const statuses: IndexStatus[] = db.indexStatus();
  const stats: DatabaseStats = db.stats();
  const dumpRep: DumpReport = db.dump('/tmp/x.json');
  const tick: WatchTick = db.tickWatches(1000);
  const code: ErrorCode = 'ENTITY_NOT_FOUND';
  const sameCode: ProtocolErrorCode = code;
  void [batchRes, report, change, current, row, edges, answer, asyncAnswer, checked, checkedAsync, statuses, stats, dumpRep, tick, sameCode];
  db.close();
}
`;
}

for (const res of RESOLUTIONS) {
  test(`packed consumer typechecks cleanly under ${res.name} with no @types/node`, () => {
    const sub = join(consumerDir, `case-${res.name}`);
    mkdirSync(sub, { recursive: true });
    writeFileSync(join(sub, 'consumer.ts'), consumerSource());
    writeFileSync(
      join(sub, 'tsconfig.json'),
      JSON.stringify({
        compilerOptions: {
          strict: true,
          noEmit: true,
          target: 'es2022',
          module: res.module,
          moduleResolution: res.moduleResolution,
          types: [],
        },
        include: ['consumer.ts'],
      }),
    );
    execFileSync(process.execPath, [tscBin, '-p', join(sub, 'tsconfig.json')], {
      cwd: consumerDir,
      encoding: 'utf8',
    });
  });
}
