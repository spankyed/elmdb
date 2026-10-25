import { defineConfig } from 'vitest/config';
import { defaultServerConditions } from 'vite';

// Vitest's own defaults: Vite's server conditions without 'module'
const conditions = ['@apackdev/source', ...defaultServerConditions.filter((c) => c !== 'module')];

export default defineConfig({
  resolve: { conditions },
  ssr: { resolve: { conditions } },
  test: {
    // A worker exits when its vitest does, rather than running on orphaned
    setupFiles: ['../apack-testing/src/vitest-exit-with-parent.ts'],
    // Large (`SIZE_MS` × `DEADLINE_FACTOR`, scripts/lib/unit-suites.ts): the harness's halves that open real LMDB
    // environments and kill child processes
    testTimeout: 120_000,
    hookTimeout: 120_000,
    include: ['tests/**/*.integration.spec.ts'],
    exclude: ['**/node_modules/**'],
  },
});
