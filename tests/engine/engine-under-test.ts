// The engine the contract specs run against. The specs only reach it through this file, so the same
// specs covered the module-level engine (before Phase 6) and cover the engine instance.
import { createEarsEngine, type EarsEngine, type PersistenceSink } from '../../src/index.ts';

export interface EngineUnderTestOptions {
  isEntityType: (name: string) => boolean;
  persistence?: PersistenceSink;
}

export function freshEngine(options: EngineUnderTestOptions): EarsEngine {
  return createEarsEngine(options);
}

export type EngineUnderTest = EarsEngine;
