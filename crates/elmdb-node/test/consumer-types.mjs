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

/** @type {Array<{ name: string, module: string, moduleResolution: string }>} */
const RESOLUTIONS = [
  { name: 'nodenext', module: 'nodenext', moduleResolution: 'nodenext' },
  { name: 'node16', module: 'node16', moduleResolution: 'node16' },
  { name: 'bundler', module: 'esnext', moduleResolution: 'bundler' },
];

/** @type {string} */
let consumerDir;
/** @type {string[]} */
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

/** TypeScript consumer exercising `elmdb`, `elmdb/client`, and every `elmdb/protocol/*` declaration without `@types/node` */
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
  Unit,
  __napiBindingTarget,
  type Answers,
  type Batch,
  type BatchResult,
  type BuilderCommit,
  type ChangeSet,
  type CheckReport,
  type ErrorCode,
  type IndexStatus,
  type OpenOptions,
  type OpenReport,
  type PurgeTarget,
  type Reads,
  type Restore,
  type Schema,
  type SchemaChange,
  type Stats,
  type Written,
} from 'elmdb';
import { Client, ClientUnit, type Codec } from 'elmdb/client';
import type { Client as ClientFromJs } from 'elmdb/client.js';
import type { Batch as ProtocolBatch } from 'elmdb/protocol/Batch.js';
import type { ErrorCode as ProtocolErrorCode } from 'elmdb/protocol/ErrorCode';
${protocolExports}

export function exerciseBinding(dir: string, schema: Schema, bytes: Uint8Array): void {
  const options: OpenOptions = { readOnly: false, schema, syncs: false };
  const db: Database = Database.open(dir, options);
  const target: 'native' | 'wasm32-wasi' | 'wasm32-wasip1' = __napiBindingTarget;
  void target;

  const writtenBytes: Uint8Array = db.write(bytes, true);
  const readBytes: Uint8Array = db.read(bytes);
  const asyncBytes: Promise<Uint8Array> = db.readAsync(bytes);
  const checkpoint: Uint8Array | null = db.checkpoint('cursor');
  const changed: Uint8Array = db.changedSince(['Note'], bytes, 100);
  db.bulkLoad(bytes);
  const replaced: number | null = db.replacedElsewhere();
  const adopted: boolean = db.adoptReplacement(replaced ?? 0);
  const report: OpenReport = db.openReport;
  const restored: Restore | null = report.restored;
  const changes: SchemaChange[] = db.setSchema(schema);
  const checked: CheckReport = db.check();
  const statuses: IndexStatus[] = db.indexStatus();
  const stats: Stats = db.stats();
  const purge: PurgeTarget = { type: 'Old' };
  db.purge(purge);
  db.hearBuilderCommits((commit: BuilderCommit) => {
    const seq: number = commit.sequence;
    const first: number | undefined = commit.touched[0];
    void seq;
    void first;
  });

  const unit: Unit = db.begin();
  const unitWritten: Uint8Array = unit.write(bytes);
  const unitRead: Uint8Array = unit.read(bytes);
  unit.setCheckpoint('cursor', bytes);
  const unitCheckpoint: Uint8Array | null = unit.checkpoint('cursor');
  const pending: Uint8Array = unit.pendingChanges();
  const unitChanged: Uint8Array = unit.changedSince(['Note'], bytes, 10);
  const committed: Uint8Array = unit.commit(false);
  unit.abort();
  db.close();

  void [
    writtenBytes, readBytes, asyncBytes, checkpoint, changed, adopted, restored,
    changes, checked, statuses, stats, unitWritten, unitRead, unitCheckpoint,
    pending, unitChanged, committed,
  ];

  // @ts-expect-error sequence is number, not string
  const wrongSequence: string = db.sequence;
  // @ts-expect-error write expects Uint8Array, not string
  db.write('not-bytes');
  // @ts-expect-error adoptReplacement expects number, not string
  db.adoptReplacement('0');
  // @ts-expect-error ErrorCode is a closed union, not any string
  const wrongCode: ErrorCode = 'NOT_A_REAL_CODE';
  void [wrongSequence, wrongCode];
}

export async function exerciseClient(db: Database, codec: Codec, reads: Reads, batch: Batch): Promise<void> {
  const client: ClientFromJs = new Client(db, codec);
  const answers: Answers = client.read(reads);
  const asyncAnswers: Answers = await client.readAsync(reads);
  const written: Written = client.write(batch, false);
  const unit: ClientUnit = client.begin();
  const unitAnswers: Answers = unit.read(reads);
  const batchResult: BatchResult = unit.write(batch);
  const pending: ChangeSet = unit.pendingChanges();
  const committed: ChangeSet = unit.commit(true);
  unit.abort();

  const sameBatch: ProtocolBatch = batch;
  const code: ProtocolErrorCode = 'VALUE_INVALID';
  void [answers, asyncAnswers, written, unitAnswers, batchResult, pending, committed, sameBatch, code];

  // @ts-expect-error Codec.pack must return Uint8Array, not string
  const badCodec: Codec = { pack: () => 'bad', unpack: () => null, unpackMaps: () => null };
  void badCodec;
}
`;
}

for (const { name, module, moduleResolution } of RESOLUTIONS) {
  test(`packed elmdb compiles without @types/node under strict ${name}`, () => {
    const caseDir = join(consumerDir, `case-${name}`);
    mkdirSync(caseDir, { recursive: true });
    writeFileSync(join(caseDir, 'package.json'), JSON.stringify({ private: true, type: 'module' }));
    writeFileSync(
      join(caseDir, 'tsconfig.json'),
      JSON.stringify({
        compilerOptions: {
          target: 'ES2022',
          lib: ['ES2022'],
          module,
          moduleResolution,
          strict: true,
          noImplicitAny: true,
          exactOptionalPropertyTypes: true,
          noUncheckedIndexedAccess: true,
          verbatimModuleSyntax: true,
          skipLibCheck: false,
          noEmit: true,
          types: [],
        },
        include: ['consumer.ts'],
      }),
    );
    writeFileSync(join(caseDir, 'consumer.ts'), consumerSource());
    execFileSync(process.execPath, [tscBin, '-p', caseDir], { cwd: caseDir, encoding: 'utf8' });
  });
}
