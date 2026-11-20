import { defineConfig } from 'vitest/config';

// EARS' own test configuration: its specs and benchmarks, and nothing of a program that runs them. A program that runs
// the suite under bounds of its own adds them to this, as `vitest.config.ts` beside it does.
export default defineConfig({
  test: {
    // Run alone (`vitest run --config vitest.ears.config.ts`), a test or hook is bounded by the clock: a minute, since
    // the model-based specs and the two-process convergence run past vitest's 5s default on a quiet machine
    testTimeout: 60_000,
    hookTimeout: 60_000,
    // Test files run in parallel: each worker's tests create their own EARS engines, each on a database of its own
    fileParallelism: true,
    benchmark: { include: ['bench/**/*.bench.ts'] },
    include: ['tests/**/*.spec.ts'],
    exclude: ['**/node_modules/**'],
  },
});
