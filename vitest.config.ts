import { defineConfig } from 'vitest/config';

// Small (`SIZE_MS` × `DEADLINE_FACTOR`, scripts/lib/unit-suites.ts). Declared rather than left to vitest's 5s
// default, which is *tighter* than the size allows: under the chain's three lanes a 5.8s typecheck in
// @apackdev/sdk crossed it and reported a hang where the size had headroom to spare.
//
// One project, as every host config is: the host pool runs this package as the project its workspace names, and
// a `projects` list here would be ignored there. The engine's contract on an LMDB store is the integration half's.
export default defineConfig({
  test: {
    testTimeout: 30_000,
    hookTimeout: 30_000,
    // A worker exits when its vitest does, rather than running on orphaned
    setupFiles: ['../../scripts/lib/vitest-exit-with-parent.ts'],
    // Test files run in parallel: each worker's tests create their own EARS engines
    fileParallelism: true,
    benchmark: { include: ['bench/**/*.bench.ts'] },
    // The harness's LMDB and process halves run in the integration pool (vitest.integration.config.ts)
    include: ['tests/**/*.spec.ts'],
    exclude: ['**/*.integration.spec.ts', '**/node_modules/**'],
  },
});
