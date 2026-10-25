import { defineConfig } from 'vitest/config';

export default defineConfig({
  test: {
    // A worker exits when its vitest does, rather than running on orphaned
    setupFiles: ['../../scripts/lib/vitest-exit-with-parent.ts'],
    // Small (`SIZE_MS` × `DEADLINE_FACTOR`, scripts/lib/unit-suites.ts). Declared rather than left to vitest's 5s
    // default, which is *tighter* than the size allows: under the chain's three lanes a 5.8s typecheck in
    // @apackdev/sdk crossed it and reported a hang where the size had headroom to spare.
    testTimeout: 30_000,
    hookTimeout: 30_000,
    include: ['tests/**/*.spec.ts'],
    // Test files run in parallel: each worker's tests create their own EARS engines
    fileParallelism: true,
    benchmark: { include: ['bench/**/*.bench.ts'] },
  },
});
