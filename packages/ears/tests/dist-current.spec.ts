/**
 * `scripts/dist-current.mjs`, which a program linking this checkout asks before it uses `dist`: over a package in a
 * repository of its own, each way the checkout can move away from the build is answered as not current.
 */
import { execFileSync, spawnSync } from 'node:child_process';
import * as fs from 'node:fs';
import * as path from 'node:path';
import { describe, expect, it } from 'vitest';
import { tempDirs, writeFiles } from './_support/packed.ts';

const SCRIPT = path.resolve(import.meta.dirname, '..', 'scripts', 'dist-current.mjs');
const tempDir = tempDirs();

/** A repository holding a package at `pkg/`, committed, with a `dist` built beside its source */
function checkout(): { pkg: string; git: (...args: string[]) => string } {
  const root = tempDir('ears-dist-current-');
  // No config of the machine's: a test's commits are made the same way everywhere
  const env = { ...process.env, GIT_CONFIG_NOSYSTEM: '1', GIT_CONFIG_GLOBAL: path.join(root, 'no-gitconfig') };
  const git = (...args: string[]) => execFileSync('git', ['-c', 'user.name=t', '-c', 'user.email=t@t', ...args], { cwd: root, env, encoding: 'utf8' });
  writeFiles(root, {
    '.gitignore': 'pkg/dist/\npkg/.temp/\n',
    'pkg/package.json': '{}',
    'pkg/src/index.ts': 'export const a = 1;\n',
    'pkg/dist/index.js': 'export const a = 1;\n',
    'other/file.txt': 'outside the package\n',
  });
  git('init', '-q');
  git('add', '-A');
  git('commit', '-q', '-m', 'first');
  return { pkg: path.join(root, 'pkg'), git };
}

/** The script's answer for `pkg`: whether `dist` is current, and what it said */
function ask(pkg: string, ...flags: string[]): { current: boolean; said: string } {
  const { status, stdout, stderr } = spawnSync(process.execPath, [SCRIPT, ...flags, pkg], { encoding: 'utf8' });
  return { current: status === 0, said: `${stdout}${stderr}` };
}

describe('dist-current', () => {
  it('answers current once a clean checkout is stamped, and keeps answering it while nothing moves', () => {
    const { pkg, git } = checkout();
    expect(ask(pkg).current, 'no stamp yet').toBe(false);
    expect(ask(pkg, '--write').current).toBe(true);
    expect(ask(pkg).current).toBe(true);
    // A change outside the package is not the package's
    fs.writeFileSync(path.join(pkg, '..', 'other', 'file.txt'), 'changed\n');
    git('commit', '-qam', 'outside');
    expect(ask(pkg).current).toBe(true);
  });

  it('answers not current for a change not committed, made after the stamp or before it', () => {
    const { pkg } = checkout();
    ask(pkg, '--write');
    fs.writeFileSync(path.join(pkg, 'src', 'index.ts'), 'export const a = 2;\n');
    expect(ask(pkg)).toEqual({ current: false, said: expect.stringContaining('changes not committed') });
    // Stamped dirty: even once the tree is clean again, no tree id names what was built
    ask(pkg, '--write');
    fs.writeFileSync(path.join(pkg, 'src', 'index.ts'), 'export const a = 1;\n');
    expect(ask(pkg)).toEqual({ current: false, said: expect.stringContaining('built from a tree with changes not committed') });
  });

  it('answers not current for an untracked file in the package', () => {
    const { pkg } = checkout();
    ask(pkg, '--write');
    fs.writeFileSync(path.join(pkg, 'src', 'new.ts'), '');
    expect(ask(pkg).current).toBe(false);
  });

  it('answers not current once HEAD holds another tree of the package, and current again on the stamped one', () => {
    const { pkg, git } = checkout();
    ask(pkg, '--write');
    fs.writeFileSync(path.join(pkg, 'src', 'index.ts'), 'export const a = 2;\n');
    git('commit', '-qam', 'second');
    expect(ask(pkg)).toEqual({ current: false, said: expect.stringContaining('HEAD holds') });
    git('checkout', '-q', 'HEAD~1');
    expect(ask(pkg).current).toBe(true);
  });

  it('answers not current for a stamp another TypeScript wrote, or for no dist', () => {
    const { pkg } = checkout();
    ask(pkg, '--write');
    const stamp = path.join(pkg, '.temp', 'dist-stamp.json');
    fs.writeFileSync(stamp, JSON.stringify({ ...JSON.parse(fs.readFileSync(stamp, 'utf8')), typescript: '0.0.0' }));
    expect(ask(pkg)).toEqual({ current: false, said: expect.stringContaining('TypeScript 0.0.0') });
    ask(pkg, '--write');
    fs.rmSync(path.join(pkg, 'dist'), { recursive: true });
    expect(ask(pkg)).toEqual({ current: false, said: expect.stringContaining('not built') });
  });
});
