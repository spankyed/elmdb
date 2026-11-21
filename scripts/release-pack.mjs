// Packs the binding as npm installs it: one package per target, `@elmdb/binding-<platform>`, holding that target's
// `.node` file and the notices its binary's licenses ask a redistribution to carry, and the binding's own package, `elmdb`, which
// holds the loader, the declarations, the thin client and the protocol and no binary, and names each platform package as
// an optional dependency. npm installs the one whose `os`, `cpu` and `libc` match the machine and skips the rest, so a
// consumer gets one binary; napi's loader (`crates/elmdb-node/index.js`) takes the binary from its own directory first,
// as an in-repo build leaves it, and else from the platform package. Run by `.github/workflows/release.yml`:
//
//   node scripts/release-pack.mjs check
//   node scripts/release-pack.mjs pack --binaries <dir> --out <dir> [--release-url <url>]
//
// `check` fails unless `crates/elmdb-node/package.json`'s `optionalDependencies` name exactly the platform package of
// each of its `napi.targets`, each at exactly its own version (`cargo xtask check-versions` holds the same). `pack`
// writes each platform's package from `<binaries>/elmdb.<platform>.node`, and the binding's, into `<out>`.
//
// **The source manifest names versions; a commit's prerelease names URLs.** The versions are what the npm registry
// would resolve, once the packages are published there. Nothing is yet: with `--release-url`, `pack` writes the
// binding's package alone, naming each platform package by `<url>/elmdb-binding-<platform>-<version>.tgz`, the release the
// platform packages are attached to beside it (`pkg-<commit>`), so installing the binding by its URL installs that
// commit's binary for the machine and nothing else, and the lockfile records each URL with its integrity.
//
// **The platform packages are scoped**, `napi.packageName` naming them `@elmdb/binding-<platform>`, which napi's
// generated loader requires. Only the scope's owner can publish under `@elmdb`, so no one else can publish a package
// by one of these names, which an install of elmdb would fetch as its optional dependency and load: an unscoped
// name, not yet on the registry, anyone could.
import { spawnSync } from 'node:child_process';
import * as fs from 'node:fs';
import * as os from 'node:os';
import * as path from 'node:path';
import { fileURLToPath } from 'node:url';
import { parseArgs } from 'node:util';

/** The binding's package, whose manifest and files this packs */
export const BINDING_DIR = path.join(path.dirname(fileURLToPath(import.meta.url)), '..', 'crates', 'elmdb-node');

/**
 * Each target the binding is built for, by the name napi's loader gives its platform (`elmdb.<platform>.node`, and the
 * package `@elmdb/binding-<platform>`), with the `os`, `cpu` and `libc` npm installs that package on. A target missing here is
 * one this script can't pack, and `check` says so rather than leaving it out
 */
export const TARGETS = {
  'aarch64-apple-darwin': { platform: 'darwin-arm64', os: 'darwin', cpu: 'arm64' },
  'x86_64-apple-darwin': { platform: 'darwin-x64', os: 'darwin', cpu: 'x64' },
  'x86_64-unknown-linux-gnu': { platform: 'linux-x64-gnu', os: 'linux', cpu: 'x64', libc: 'glibc' },
  'aarch64-unknown-linux-gnu': { platform: 'linux-arm64-gnu', os: 'linux', cpu: 'arm64', libc: 'glibc' },
  'x86_64-unknown-linux-musl': { platform: 'linux-x64-musl', os: 'linux', cpu: 'x64', libc: 'musl' },
  'aarch64-unknown-linux-musl': { platform: 'linux-arm64-musl', os: 'linux', cpu: 'arm64', libc: 'musl' },
  'x86_64-pc-windows-msvc': { platform: 'win32-x64-msvc', os: 'win32', cpu: 'x64' },
};

/** The notices each binary goes with: the crates', LMDB's, the Rust standard library's and elmdb's own licenses */
export const NOTICES = ['LICENSE-MIT', 'LICENSE-APACHE', 'LICENSE-OPENLDAP', 'THIRD-PARTY-NOTICES', 'COPYRIGHT-rust-library.html'];

/** The scope every platform package is published under, which only elmdb's owner can publish in */
export const SCOPE = '@elmdb/';

/** The file `npm pack` writes for a package: a scope's `@` dropped and its `/` made a `-` */
export const tarballName = (name, version) => `${name.replace(/^@/, '').replace('/', '-')}-${version}.tgz`;

/** The platforms of the manifest's `napi.targets`, in its order; a target this script doesn't know fails */
export function platforms(manifest) {
  const prefix = manifest.napi?.packageName;
  if (typeof prefix !== 'string' || !prefix.startsWith(SCOPE)) {
    throw new Error(`package.json's napi.packageName is ${JSON.stringify(prefix)}, and is to name a package in ${SCOPE}`);
  }
  return (manifest.napi?.targets ?? []).map((target) => {
    if (!TARGETS[target]) throw new Error(`package.json's napi.targets names ${target}, which release-pack.mjs doesn't know`);
    return { target, ...TARGETS[target], name: `${prefix}-${TARGETS[target].platform}` };
  });
}

/** Fails unless `optionalDependencies` names each target's platform package, and only those, at exactly the version */
export function check(manifest) {
  const named = manifest.optionalDependencies ?? {};
  const expected = platforms(manifest).map(({ name }) => name);
  const problems = [];
  for (const name of expected) {
    if (!(name in named)) problems.push(`${name} is missing`);
    else if (named[name] !== manifest.version) problems.push(`${name} is at ${JSON.stringify(named[name])}`);
  }
  for (const name of Object.keys(named)) {
    if (!expected.includes(name)) problems.push(`${name} is the package of no target in napi.targets`);
  }
  if (problems.length > 0) {
    throw new Error(
      `crates/elmdb-node/package.json's optionalDependencies are to name each target's platform package at exactly ${manifest.version}: ${problems.join('; ')}`,
    );
  }
}

/** The manifest of one platform's package */
export function platformManifest(main, { platform, os: osName, cpu, libc, name }) {
  const binary = `${main.napi.binaryName}.${platform}.node`;
  return {
    name,
    version: main.version,
    description: `elmdb's prebuilt binding for ${platform}, which the package elmdb loads. Install elmdb, not this.`,
    license: main.license,
    repository: main.repository,
    os: [osName],
    cpu: [cpu],
    ...(libc ? { libc: [libc] } : {}),
    main: binary,
    files: [binary, ...NOTICES],
    engines: main.engines,
    publishConfig: { access: 'public' },
  };
}

/** The binding's manifest as packed: with `releaseUrl`, each platform package named by its tarball there */
export function mainManifest(main, releaseUrl) {
  check(main);
  if (!releaseUrl) return main;
  const optionalDependencies = Object.fromEntries(
    Object.keys(main.optionalDependencies).map((name) => [name, `${releaseUrl}/${tarballName(name, main.version)}`]),
  );
  return { ...main, optionalDependencies };
}

function npm(args, cwd) {
  const run = spawnSync('npm', args, { cwd, encoding: 'utf8', shell: process.platform === 'win32' });
  if (run.status !== 0) throw new Error(`npm ${args.join(' ')} in ${cwd} failed:\n${run.stderr}`);
  return JSON.parse(run.stdout);
}

function packStaged(name, files, manifest, out) {
  const stage = fs.mkdtempSync(path.join(os.tmpdir(), `${name.replace(/^@/, '').replace('/', '-')}-`));
  for (const [to, from] of files) {
    fs.mkdirSync(path.dirname(path.join(stage, to)), { recursive: true });
    fs.copyFileSync(from, path.join(stage, to));
  }
  fs.writeFileSync(path.join(stage, 'package.json'), `${JSON.stringify(manifest, null, 2)}\n`);
  const [{ filename, files: packed }] = npm(['pack', '--json', '--pack-destination', out], stage);
  fs.rmSync(stage, { recursive: true });
  // npm packs what `files` names and nothing it can't find; a file named and missing would ship a package without it
  const missing = manifest.files.filter((file) => !file.includes('*') && !packed.some(({ path: held }) => held === file));
  if (missing.length > 0) throw new Error(`${filename} lacks ${missing.join(', ')}`);
  return path.join(out, filename);
}

/** Packs the platform packages and the binding's into `out`, or the binding's alone with `releaseUrl`; their paths */
export function pack({ binaries, out, releaseUrl }) {
  const main = JSON.parse(fs.readFileSync(path.join(BINDING_DIR, 'package.json'), 'utf8'));
  const manifest = mainManifest(main, releaseUrl);
  fs.mkdirSync(out, { recursive: true });
  const tarballs = [];
  if (!releaseUrl) {
    for (const target of platforms(main)) {
      const platformPackage = platformManifest(main, target);
      const files = [
        [platformPackage.main, path.join(binaries, platformPackage.main)],
        ...NOTICES.map((file) => [file, path.join(BINDING_DIR, file)]),
      ];
      for (const [, from] of files) if (!fs.existsSync(from)) throw new Error(`${from} isn't there to pack`);
      tarballs.push(packStaged(target.name, files, platformPackage, out));
    }
  }
  // The binding's files as npm would pack them from its directory, which holds no binary: one there would be a
  // build's, and the package's `files` names none
  const [{ files }] = npm(['pack', '--dry-run', '--json'], BINDING_DIR);
  const held = files.map(({ path: file }) => file).filter((file) => file !== 'package.json');
  const binary = held.find((file) => file.endsWith('.node'));
  if (binary) throw new Error(`The binding's package would hold ${binary}; its binaries are the platform packages'`);
  tarballs.push(packStaged(main.name, held.map((file) => [file, path.join(BINDING_DIR, file)]), manifest, out));
  return tarballs;
}

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  const { positionals, values } = parseArgs({
    allowPositionals: true,
    options: { binaries: { type: 'string' }, out: { type: 'string' }, 'release-url': { type: 'string' } },
  });
  const usage = 'usage: node scripts/release-pack.mjs check | pack --binaries <dir> --out <dir> [--release-url <url>]';
  if (positionals[0] === 'check') {
    check(JSON.parse(fs.readFileSync(path.join(BINDING_DIR, 'package.json'), 'utf8')));
  } else if (positionals[0] === 'pack' && values.out && (values.binaries || values['release-url'])) {
    const binaries = values.binaries && path.resolve(values.binaries);
    for (const tarball of pack({ binaries, out: path.resolve(values.out), releaseUrl: values['release-url'] })) {
      console.log(tarball);
    }
  } else {
    console.error(usage);
    process.exit(2);
  }
}
