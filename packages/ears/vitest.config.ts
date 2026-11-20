import { defineConfig } from 'vitest/config';

// EARS' fast half: every spec but the integration half's (`vitest.integration.config.ts`, which extends this), and the
// benchmarks (`vitest bench`).
export default defineConfig({
  test: {
    // A test or hook is bounded by the clock: a minute, since the model-based specs and the two-process convergence run
    // past vitest's 5s default on a quiet machine
    testTimeout: 60_000,
    hookTimeout: 60_000,
    // Test files run in parallel: each worker's tests create their own EARS engines, each on a database of its own
    fileParallelism: true,
    benchmark: { include: ['bench/**/*.bench.ts'] },
    include: ['tests/**/*.spec.ts'],
    exclude: ['**/node_modules/**', '**/*.integration.spec.ts'],
  },
});
