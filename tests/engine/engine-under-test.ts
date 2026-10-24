// The engine the contract specs run against. The specs only reach it through this file, so the same
// specs covered the module-level engine (before Phase 6) and cover the engine instance.
import { createEarsEngine, type EARSRuntimeDeps, type EarsEngine, type PersistenceSink } from '../../src/index.ts';

export interface EngineUnderTestOptions {
  isEntityType: (name: string) => boolean;
  isRelationKind: (name: string) => boolean;
  isRole: (name: string) => boolean;
  rowRules?: EARSRuntimeDeps['rowRules'];
  persistence?: PersistenceSink;
  warn?: EARSRuntimeDeps['warn'];
  kv?: EARSRuntimeDeps['kv'];
}

export function freshEngine(options: EngineUnderTestOptions): EarsEngine {
  return createEarsEngine(options);
}

export type EngineUnderTest = EarsEngine;
