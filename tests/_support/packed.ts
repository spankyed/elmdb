/**
 * This package as a consumer installs it: its staged tree npm-packed, the tarball unpacked into a consumer's
 * `node_modules` with the package's dependencies (`elmdb`, the binding, among them) installed beside it, and the
 * compilers a consumer may use. What the package is named and what it exports are read from its own manifest, so
 * nothing here names the package.
 */
import { execFile } from 'node:child_process';
import * as fs from 'node:fs';
import * as os from 'node:os';
import * as path from 'node:path';
import { fileURLToPath } from 'node:url';

/** This package's directory */
export const PACKAGE_DIR = fileURLToPath(new URL('../..', import.meta.url));

interface Manifest {
  name: string;
  exports: Record<string, unknown>;
  dependencies?: Record<string, string>;
  scripts?: Record<string, string>;
}

export const MANIFEST = JSON.parse(fs.readFileSync(path.join(PACKAGE_DIR, 'package.json'), 'utf8')) as Manifest;

/**
 * Where the package's build stages what npm publishes: the built `dist` and a manifest derived from the workspace
 * one, with no source branches in its exports
 */
export const STAGED_TREE = path.join(PACKAGE_DIR, 'publish');

/**
 * The compilers a consumer may use: the TypeScript this package is built with, and the oldest it supports. The floor
 * isn't a dependency of this package, since a second `typescript` installed beside the first takes its `tsc` on the
 * workspace's path: the program running the suite says where it is, as `EARS_TYPESCRIPT_FLOOR`, the directory of
 * that `typescript` package. A run that doesn't is refused rather than checking one compiler and passing.
 */
export function compilers(): Record<'current' | 'floor', string> {
  const floor = process.env.EARS_TYPESCRIPT_FLOOR;
  if (!floor) {
    throw new Error('EARS_TYPESCRIPT_FLOOR is unset: set it to the directory of the oldest TypeScript this package supports (5.7), which its test config names');
  }
  const version = (JSON.parse(fs.readFileSync(path.join(floor, 'package.json'), 'utf8')) as { version: string }).version;
  if (!version.startsWith('5.7.')) throw new Error(`EARS_TYPESCRIPT_FLOOR is TypeScript ${version}, not the floor, 5.7`);
  return {
    current: path.join(packageDirOf('typescript'), 'bin', 'tsc'),
    floor: path.join(floor, 'bin', 'tsc'),
  };
}

/** Where `name` is installed for this package: Node's lookup, up the `node_modules` directories from here */
export function packageDirOf(name: string): string {
  for (let dir = PACKAGE_DIR; ; dir = path.dirname(dir)) {
    const candidate = path.join(dir, 'node_modules', ...name.split('/'));
    if (fs.existsSync(path.join(candidate, 'package.json'))) return fs.realpathSync(candidate);
    if (path.dirname(dir) === dir) throw new Error(`${name} is not installed for ${MANIFEST.name}`);
  }
}

/**
 * Runs a command to its exit, answering its output, or rejecting with it when it exits with an error. No clock bounds
 * it: the program running the suite bounds a test by its progress.
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

/**
 * The staged tree, refused when it isn't there. That it is the build of the source as it is now is the runner's to
 * see to, as it builds the package before the suites that read its build: apack's chain runs this half after
 * `packages:ensure`, which rebuilds a package whose inputs changed. An age compared here would refuse a build
 * whose sources a checkout only touched.
 */
function stagedTree(): string {
  const staged = path.join(STAGED_TREE, 'package.json');
  if (!fs.existsSync(staged)) {
    const build = MANIFEST.scripts?.['build:package'] ? 'npm run build:package' : 'the package\'s build';
    throw new Error(`${MANIFEST.name} isn't built: no ${staged}. Run ${build} in ${PACKAGE_DIR}`);
  }
  return STAGED_TREE;
}

export interface Consumer {
  /** The consumer's directory, which holds its `node_modules` */
  dir: string;
  /** Where the packed package is unpacked */
  installed: string;
  remove(): void;
}

/**
 * A consumer in a temporary directory with the packed package installed, as npm installs it from the registry, and each
 * of its dependencies linked beside it from this package's install
 */
export async function installPacked(): Promise<Consumer> {
  const staged = stagedTree();
  const dir = fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), 'ears-consumer-')));
  const remove = () => fs.rmSync(dir, { recursive: true, force: true });
  try {
    const packed = await run('npm', ['pack', staged, '--pack-destination', dir, '--json', '--ignore-scripts'], dir);
    if (packed.code !== 0) throw new Error(`npm pack failed:\n${packed.output}`);
    const [{ filename }] = JSON.parse(packed.output.slice(packed.output.indexOf('['))) as Array<{ filename: string }>;
    const installed = path.join(dir, 'node_modules', ...MANIFEST.name.split('/'));
    fs.mkdirSync(installed, { recursive: true });
    const unpacked = await run('tar', ['-xzf', path.join(dir, path.basename(filename)), '-C', installed, '--strip-components', '1'], dir);
    if (unpacked.code !== 0) throw new Error(`tar failed:\n${unpacked.output}`);
    for (const dependency of Object.keys(MANIFEST.dependencies ?? {})) {
      const target = path.join(dir, 'node_modules', ...dependency.split('/'));
      fs.mkdirSync(path.dirname(target), { recursive: true });
      fs.symlinkSync(packageDirOf(dependency), target, 'junction');
    }
    fs.writeFileSync(path.join(dir, 'package.json'), JSON.stringify({ name: 'consumer', private: true, type: 'module' }));
    return { dir, installed, remove };
  } catch (error) {
    remove();
    throw error;
  }
}

/** The packed manifest's exports that a consumer imports as code, as specifiers: each with declarations */
export function codeExports(consumer: Consumer): string[] {
  const manifest = JSON.parse(fs.readFileSync(path.join(consumer.installed, 'package.json'), 'utf8')) as Manifest;
  return Object.entries(manifest.exports)
    .filter(([key, target]) => !key.endsWith('.json') && typeof target === 'object' && target !== null)
    .map(([key]) => `${manifest.name}${key.slice(1)}`);
}

export type ModuleResolution = 'node16' | 'bundler';
export const MODULE_RESOLUTIONS: readonly ModuleResolution[] = ['node16', 'bundler'];

/**
 * The compiler options of a consumer with no Node types, under a module resolution. Declarations are checked too
 * (`skipLibCheck` off): a declaration naming a type only Node's provide fails to compile, where skipped it would read
 * as `any`.
 */
export function consumerOptions(moduleResolution: ModuleResolution): Record<string, unknown> {
  return {
    target: 'ES2022', module: moduleResolution === 'node16' ? 'node16' : 'esnext', moduleResolution,
    strict: true, noImplicitAny: true, skipLibCheck: false, noEmit: true, types: [], lib: ['ES2022'],
  };
}
