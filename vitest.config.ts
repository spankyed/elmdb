import { defineConfig, mergeConfig } from 'vitest/config';
import ears from './vitest.ears.config.ts';

// EARS' suite as apack runs it: EARS' own configuration (`vitest.ears.config.ts`, which names nothing of apack's), with
// the bounds every apack config holds a test to added here. So EARS' suite needs no file of another package, and this
// file stays the one apack's tools find (the root config's projects, `suite-timeouts.spec.ts`, `npm test`).
//
// One project, as every host config is: the host pool runs this package as the project its workspace names, and
// a `projects` list here would be ignored there. This is the fast half: the specs that pack and compile the built
// package are the integration half's (`vitest.integration.config.ts`).
export default mergeConfig(ears, defineConfig({
  test: {
    exclude: ['**/*.integration.spec.ts'],
    // Small (`SIZE_MS` × `DEADLINE_FACTOR`, scripts/lib/unit-suites.ts): the CPU one test may spend. A test that
    // needs more declares a reviewed `meta.cpuBudgetMs` (`suite-timeouts.spec.ts`)
    provide: { 'apack.testCpuMs': 30_000 },
    // Vitest's clock is off: the progress guard in setupFiles fails a test or hook whose process tree stalls, and a
    // test that spends the CPU budget above (`vitest-progress-guard.ts`, `@apackdev/testing`)
    testTimeout: 0,
    hookTimeout: 0,
    // A worker exits when its vitest does, rather than running on orphaned; git reads no config of the machine's
    setupFiles: ['../apack-testing/src/vitest-exit-with-parent.ts', '../apack-testing/src/vitest-progress-guard.ts', '../apack-testing/src/vitest-git-config.ts'],
  },
}));
