import { fileURLToPath } from 'node:url';
import { defineConfig, mergeConfig } from 'vitest/config';
import fast from './vitest.config.ts';

// EARS' integration half: the specs that pack the built package into a consumer and compile it
// (`*.integration.spec.ts`), so `dist` is built first, under the fast half's configuration with its choice of files
// replaced. mergeConfig joins lists, so the fast half's own are left out rather than overridden.
const { include: _fastSpecs, exclude: _fastExclusions, ...shared } = fast.test ?? {};

export default mergeConfig({ ...fast, test: shared }, defineConfig({
  test: {
    include: ['tests/**/*.integration.spec.ts'],
    exclude: ['**/node_modules/**'],
    // The oldest TypeScript EARS supports, which a consumer of the packed package compiles with beside the current one
    // (`tests/_support/packed.ts`): the floor's workspace, `packages/typescript-floor`, installs it
    env: { EARS_TYPESCRIPT_FLOOR: fileURLToPath(new URL('../typescript-floor/node_modules/typescript', import.meta.url)) },
  },
}));
