import { fileURLToPath } from 'node:url';
import { defineConfig, mergeConfig } from 'vitest/config';
import ears from './vitest.ears.config.ts';

// EARS' integration half as apack runs it: the specs that pack the built package into a consumer and compile it
// (`*.integration.spec.ts`), under EARS' own configuration (`vitest.ears.config.ts`) with the bounds every apack
// integration config holds a test to added here, as `vitest.config.ts` adds them to the fast half.
const { include: _everySpec, ...earsTest } = ears.test ?? {};

export default mergeConfig({ ...ears, test: earsTest }, defineConfig({
  test: {
    include: ['tests/**/*.integration.spec.ts'],
    // The oldest TypeScript EARS supports, which a consumer of the packed package compiles with beside the current
    // one: apack's floor package holds it (`packages/typescript-floor`)
    env: { EARS_TYPESCRIPT_FLOOR: fileURLToPath(new URL('../typescript-floor/node_modules/typescript', import.meta.url)) },
    // Large (`SIZE_MS` × `DEADLINE_FACTOR`, scripts/lib/unit-suites.ts): the CPU one test may spend. A test that
    // needs more declares a reviewed `meta.cpuBudgetMs` (`suite-timeouts.spec.ts`)
    provide: { 'apack.testCpuMs': 120_000 },
    // Vitest's clock is off: the progress guard in setupFiles fails a test or hook whose process tree stalls, and a
    // test that spends the CPU budget above (`vitest-progress-guard.ts`, `@apackdev/testing`)
    testTimeout: 0,
    hookTimeout: 0,
    // A worker exits when its vitest does, rather than running on orphaned; git reads no config of the machine's
    setupFiles: ['../apack-testing/src/vitest-exit-with-parent.ts', '../apack-testing/src/vitest-progress-guard.ts', '../apack-testing/src/vitest-git-config.ts'],
  },
}));
