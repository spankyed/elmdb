import { defineConfig } from 'vitest/config';

export default defineConfig({
  test: {
    // Small (`SIZE_MS`, scripts/lib/unit-suites.ts). Declared rather than left to vitest's 5s
    // default, which is *tighter* than the size allows: under the chain's three lanes a 5.8s typecheck in
    // @apack/sdk crossed it and reported a hang where the size had headroom to spare.
    testTimeout: 15_000,
    hookTimeout: 15_000,
    include: ['tests/**/*.spec.ts'],
    // Test files run in parallel: each worker's tests create their own EARS engines
    fileParallelism: true,
    benchmark: { include: ['bench/**/*.bench.ts'] },
  },
});
