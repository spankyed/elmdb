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
    // environments and kill child processes, and the engine's contract once more with every engine on an LMDB store
    // (`tests/engine/engine-under-test.ts`): what holds on memory, where the unit half runs it, holds on the app's
    // backend. The model-based spec builds its own store, one that can refuse a commit, so it runs once
    testTimeout: 120_000,
    hookTimeout: 120_000,
    include: ['tests/**/*.integration.spec.ts', 'tests/engine/**/*.spec.ts'],
    exclude: ['tests/engine/model-based.spec.ts', '**/node_modules/**'],
    env: { EARS_KV: 'lmdb' },
  },
});
