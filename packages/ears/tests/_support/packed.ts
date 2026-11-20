/**
 * This package as a consumer installs it: npm-packed, the tarball unpacked into a consumer's `node_modules` with the
 * package's dependencies and peers (`elmdb`, the binding, among them) linked beside it, and the compilers a consumer
 * may use. The package's name and exports are read from its own manifest, so nothing here names the package.
 */
import { execFile } from 'node:child_process';
import * as fs from 'node:fs';
import * as os from 'node:os';
import * as path from 'node:path';
import { fileURLToPath } from 'node:url';
import { afterAll, beforeAll } from 'vitest';

export const PACKAGE_DIR = fileURLToPath(new URL('../..', import.meta.url));

interface Manifest {
  name: string;
  exports: Record<string, unknown>;
  dependencies?: Record<string, string>;
  peerDependencies?: Record<string, string>;
}

export const readManifest = (dir: string): Manifest => JSON.parse(fs.readFileSync(path.join(dir, 'package.json'), 'utf8')) as Manifest;
export const MANIFEST = readManifest(PACKAGE_DIR);

/** An ES module package's manifest, for a consumer's directory */
export const MODULE_MANIFEST = JSON.stringify({ private: true, type: 'module' });

/** Writes each of `files`, by its path under `dir`, making the directories it needs */
export function writeFiles(dir: string, files: Record<string, string>): void {
  for (const [name, content] of Object.entries(files)) {
    fs.mkdirSync(path.dirname(path.join(dir, name)), { recursive: true });
    fs.writeFileSync(path.join(dir, name), content);
  }
}

/** Makes temporary directories, each removed when the calling file's tests are done */
export function tempDirs(): (prefix: string) => string {
  const made: string[] = [];
  afterAll(() => {
    for (const dir of made.splice(0)) fs.rmSync(dir, { recursive: true, force: true });
  });
  return (prefix) => {
    const dir = fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), prefix)));
    made.push(dir);
    return dir;
  };
}

/**
 * The compilers a consumer may use: the TypeScript this package is built with, and the oldest it supports. The floor
 * isn't a dependency, since a second `typescript` beside the first would take its `tsc` on the workspace's path: the
 * program running the suite names its directory as `EARS_TYPESCRIPT_FLOOR`, and a run without it is refused rather
 * than checking one compiler and passing.
 */
export function compilers(): Record<'current' | 'floor', string> {
  const floor = process.env.EARS_TYPESCRIPT_FLOOR;
  if (!floor) {
    throw new Error('EARS_TYPESCRIPT_FLOOR is unset: set it to the directory of the oldest TypeScript this package supports (5.7), which its test config names');
  }
  const { version } = JSON.parse(fs.readFileSync(path.join(floor, 'package.json'), 'utf8')) as { version: string };
  if (!version.startsWith('5.7.')) throw new Error(`EARS_TYPESCRIPT_FLOOR is TypeScript ${version}, not the floor, 5.7`);
  return { current: path.join(packageDirOf('typescript'), 'bin', 'tsc'), floor: path.join(floor, 'bin', 'tsc') };
}

/** Where `name` is installed for this package: Node's lookup, up the `node_modules` directories from here */
function packageDirOf(name: string): string {
  for (let dir = PACKAGE_DIR; ; dir = path.dirname(dir)) {
    const candidate = path.join(dir, 'node_modules', name);
    if (fs.existsSync(path.join(candidate, 'package.json'))) return fs.realpathSync(candidate);
    if (path.dirname(dir) === dir) throw new Error(`${name} is not installed for ${MANIFEST.name}`);
  }
}

/**
 * Runs a command to its exit, answering its exit code and output. No clock bounds it: the program running the suite
 * bounds a test by its progress.
 */
export function run(command: string, args: readonly string[], cwd: string): Promise<{ code: number; output: string }> {
  return new Promise((resolve) => {
    execFile(command, args, { cwd, encoding: 'utf8', maxBuffer: 64 * 1024 * 1024, shell: process.platform === 'win32' && command === 'npm' },
      (error, stdout, stderr) => {
        const code = error === null ? 0 : typeof error.code === 'number' ? error.code : -1;
        resolve({ code, output: `${stdout}${stderr}${code === -1 ? String(error) : ''}` });
      });
  });
}

export interface Consumer {
  /** The consumer's directory, which holds its `node_modules` */
  dir: string;
  /** Where the packed package is unpacked */
  installed: string;
}

/**
 * A consumer with the packed package installed, as npm installs it from the registry, made before the calling file's
 * tests and removed after them. What is packed is the package's directory as npm packs it, its `dist` among it; that
 * `dist` is the build of the source as it is now is the runner's to see to (`npm run build` at the root before
 * `npm test`).
 */
export function packedConsumer(): () => Consumer {
  const tempDir = tempDirs();
  let consumer: Consumer | undefined;
  beforeAll(async () => {
    if (!fs.existsSync(path.join(PACKAGE_DIR, 'dist'))) {
      throw new Error(`${MANIFEST.name} isn't built: no dist. Run npm run build at the repository's root`);
    }
    const dir = tempDir('ears-consumer-');
    const packed = await run('npm', ['pack', PACKAGE_DIR, '--pack-destination', dir, '--json', '--ignore-scripts'], dir);
    if (packed.code !== 0) throw new Error(`npm pack failed:\n${packed.output}`);
    const [{ filename }] = JSON.parse(packed.output.slice(packed.output.indexOf('['))) as Array<{ filename: string }>;
    const installed = path.join(dir, 'node_modules', MANIFEST.name);
    fs.mkdirSync(installed, { recursive: true });
    const unpacked = await run('tar', ['-xzf', path.join(dir, path.basename(filename)), '-C', installed, '--strip-components', '1'], dir);
    if (unpacked.code !== 0) throw new Error(`tar failed:\n${unpacked.output}`);
    // A peer is installed beside its dependent, as npm installs one the consumer doesn't already hold
    for (const dependency of Object.keys({ ...MANIFEST.dependencies, ...MANIFEST.peerDependencies })) {
      const target = path.join(dir, 'node_modules', dependency);
      fs.mkdirSync(path.dirname(target), { recursive: true });
      fs.symlinkSync(packageDirOf(dependency), target, 'junction');
    }
    writeFiles(dir, { 'package.json': MODULE_MANIFEST });
    consumer = { dir, installed };
  });
  return () => consumer!;
}

/** The packed manifest's exports that a consumer imports as code, each with declarations, as specifiers */
export function codeExports(consumer: Consumer): string[] {
  const { name, exports } = readManifest(consumer.installed);
  return Object.entries(exports)
    .filter(([key, target]) => !key.endsWith('.json') && typeof target === 'object' && target !== null)
    .map(([key]) => `${name}${key.slice(1)}`);
}

export const MODULE_RESOLUTIONS = ['node16', 'bundler'] as const;
export type ModuleResolution = (typeof MODULE_RESOLUTIONS)[number];

/**
 * The compiler options of a consumer with no Node types. Declarations are checked too (`skipLibCheck` off): a
 * declaration naming a type only Node's provide fails to compile, where skipped it would read as `any`.
 */
export function consumerOptions(moduleResolution: ModuleResolution): Record<string, unknown> {
  return {
    target: 'ES2022', module: moduleResolution === 'node16' ? 'node16' : 'esnext', moduleResolution,
    strict: true, noImplicitAny: true, skipLibCheck: false, noEmit: true, types: [], lib: ['ES2022'],
  };
}
