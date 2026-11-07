import { defineConfig } from 'vitest/config';

// Small (`SIZE_MS` × `DEADLINE_FACTOR`, scripts/lib/unit-suites.ts). Declared rather than left to vitest's 5s
// default, which is *tighter* than the size allows: under the chain's three lanes a 5.8s typecheck in
// @apackdev/sdk crossed it and reported a hang where the size had headroom to spare.
//
// One project, as every host config is: the host pool runs this package as the project its workspace names, and
// a `projects` list here would be ignored there. The package has no integration half.
export default defineConfig({
  test: {
    // Small (`SIZE_MS` × `DEADLINE_FACTOR`, scripts/lib/unit-suites.ts): the CPU one test may spend. A test that
    // needs more declares a reviewed `meta.cpuBudgetMs` (`suite-timeouts.spec.ts`)
    provide: { 'apack.testCpuMs': 30_000 },
    // Vitest's clock is off: the progress guard in setupFiles fails a test or hook whose process tree stalls, and a
    // test that spends the CPU budget above (`vitest-progress-guard.ts`, `@apackdev/testing`)
    testTimeout: 0,
    hookTimeout: 0,
    // A worker exits when its vitest does, rather than running on orphaned
    setupFiles: ['../apack-testing/src/vitest-exit-with-parent.ts', '../apack-testing/src/vitest-progress-guard.ts'],
    // Test files run in parallel: each worker's tests create their own EARS engines, each on a database of its own
    fileParallelism: true,
    benchmark: { include: ['bench/**/*.bench.ts'] },
    include: ['tests/**/*.spec.ts'],
    exclude: ['**/node_modules/**'],
  },
});
