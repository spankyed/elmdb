// The engine the free functions (`qx`, `tx`, `repository`, the `defineEars` facades…) act on. It holds a reference to
// an engine's query face, never data: whoever creates the engine installs it (the app when it binds, tests, tooling).
import type { EarsQuery } from './engine.ts';

let installed: EarsQuery | undefined;

/** The error an engine use with none installed throws */
export const NO_ENGINE_INSTALLED =
  'No EARS engine is installed: the app installs its engine when it binds (bindHost from @apack/sdk/runtime), ' +
  'unit tests with startTestRuntime() from @apack/sdk/testing, and tooling with ' +
  'installEngine(createEarsEngine({ isEntityType }).query) from @apack/ears';

/**
 * Makes `engine` the one the free functions act on, or none with `undefined`. Returns the engine installed
 * before, so tooling can put it back.
 */
export function installEngine(engine: EarsQuery | undefined): EarsQuery | undefined {
  const previous = installed;
  installed = engine;
  return previous;
}

/** The installed engine's query face; throws when none is installed */
export function installedEngine(): EarsQuery {
  if (!installed) throw new Error(NO_ENGINE_INSTALLED);
  return installed;
}
