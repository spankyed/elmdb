import { defineConfig } from 'vitest/config';

// Small (`SIZE_MS` × `DEADLINE_FACTOR`, scripts/lib/unit-suites.ts). Declared rather than left to vitest's 5s
// default, which is *tighter* than the size allows: under the chain's three lanes a 5.8s typecheck in
// @apackdev/sdk crossed it and reported a hang where the size had headroom to spare.
const deadlines = { testTimeout: 30_000, hookTimeout: 30_000 };
// A worker exits when its vitest does, rather than running on orphaned; each project declares it, inheriting nothing
const setupFiles = ['../../scripts/lib/vitest-exit-with-parent.ts'];

export default defineConfig({
  test: {
    ...deadlines,
    // Test files run in parallel: each worker's tests create their own EARS engines
    fileParallelism: true,
    benchmark: { include: ['bench/**/*.bench.ts'] },
    projects: [
      { test: { ...deadlines, setupFiles, name: 'ears', include: ['tests/**/*.spec.ts'] } },
      // The engine's contract once more, every engine on an LMDB store (`tests/engine/engine-under-test.ts`): what
      // holds on memory, where the other specs run, holds on the app's backend
      { test: { ...deadlines, setupFiles, name: 'ears-on-lmdb', include: ['tests/engine/**/*.spec.ts'], env: { EARS_KV: 'lmdb' } } },
    ],
  },
});
