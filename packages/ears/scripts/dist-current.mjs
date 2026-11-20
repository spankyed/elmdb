// Whether `dist` is the build of this package as the checkout holds it now, for a program that links the checkout and
// uses `dist` (apack does), and for anything here that reads `dist`.
//
//   node packages/ears/scripts/dist-current.mjs [--write] [dir]
//
// exits 0 when it is, and 1 saying why when it isn't; `--write` records the checkout as it is now as what `dist` was
// built from, and then answers as above. `dir` is the package's directory, this script's own unless given.
//
// The build (`build.mjs`) clears the stamp before it touches `dist` and writes it once every check has passed: the tree
// id of the package's directory at HEAD, whether `git status` showed it clean, and the TypeScript version that built
// it. A build of a tree with changes not committed is never current, since no tree id names what it built.
import { execFileSync } from 'node:child_process';
import * as fs from 'node:fs';
import { createRequire } from 'node:module';
import * as path from 'node:path';
import { fileURLToPath } from 'node:url';

/** This package's directory */
export const PACKAGE_DIR = fileURLToPath(new URL('..', import.meta.url));

/** Where the stamp is kept: inside the package, under `.temp/`, which git ignores and `npm pack` doesn't ship */
export const stampPath = (dir) => path.join(dir, '.temp', 'dist-stamp.json');

const git = (dir, args) => execFileSync('git', args, { cwd: dir, encoding: 'utf8', stdio: ['ignore', 'pipe', 'pipe'] }).trim();

/** The TypeScript the build runs, as it resolves it from this package */
const typescriptVersion = () => JSON.parse(fs.readFileSync(createRequire(import.meta.url).resolve('typescript/package.json'), 'utf8')).version;

/** The checkout as it is now: the package directory's tree at HEAD, whether it is clean, and the TypeScript it builds with */
export function current(dir) {
  return {
    tree: git(dir, ['rev-parse', 'HEAD:./']),
    clean: git(dir, ['status', '--porcelain', '--', '.']) === '',
    typescript: typescriptVersion(),
  };
}

/** Removes the stamp: what a build does before it touches `dist` */
export function clearStamp(dir) {
  fs.rmSync(stampPath(dir), { force: true });
}

/** Records the checkout as it is now as what `dist` was built from: what a build does once it has succeeded */
export function writeStamp(dir) {
  fs.mkdirSync(path.dirname(stampPath(dir)), { recursive: true });
  fs.writeFileSync(stampPath(dir), `${JSON.stringify(current(dir))}\n`);
}

/** Why `dist` is not the build of the checkout as it is now, or undefined when it is */
export function staleness(dir) {
  if (!fs.existsSync(path.join(dir, 'dist'))) return 'dist is not built';
  if (!fs.existsSync(stampPath(dir))) return 'dist has no stamp: no build of it succeeded since it was last cleared';
  const stamp = JSON.parse(fs.readFileSync(stampPath(dir), 'utf8'));
  const now = current(dir);
  if (!stamp.clean) return 'dist was built from a tree with changes not committed';
  if (!now.clean) return 'the package has changes not committed';
  if (stamp.tree !== now.tree) return `dist was built from tree ${stamp.tree}, and HEAD holds ${now.tree}`;
  if (stamp.typescript !== now.typescript) return `dist was built by TypeScript ${stamp.typescript}, and ${now.typescript} is installed`;
  return undefined;
}

if (process.argv[1] && fs.realpathSync(process.argv[1]) === fs.realpathSync(fileURLToPath(import.meta.url))) {
  const args = process.argv.slice(2);
  const write = args.includes('--write');
  const [given] = args.filter((arg) => arg !== '--write');
  const dir = given ? path.resolve(given) : PACKAGE_DIR;
  const why = write ? (writeStamp(dir), staleness(dir)) : staleness(dir);
  if (why === undefined) {
    console.log(`${path.relative(process.cwd(), path.join(dir, 'dist')) || 'dist'} is current`);
  } else {
    console.error(`${path.relative(process.cwd(), path.join(dir, 'dist')) || 'dist'} is not current: ${why}. Run npm run build:ears`);
    process.exitCode = 1;
  }
}
