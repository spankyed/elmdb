// The lead developer's production data as profiled 2026-10-24 (a converted copy, format 4): per entity type, its
// entities and, per attribute kind, how many hold it and their mean size serialized; the large values per kind, their
// count and mean size; the relations per kind. `loadProductionShape` writes data of this shape, which
// production.bench.ts reads; ears.bench.ts's uniform graph is the one its recorded readings were taken on
import type { EarsAdmin, EARS } from '../src/index.ts';
export const TYPES: Record<string, { entities: number; fields: Record<string, [count: number, bytes: number]> }> = {
  Message: { entities: 19803, fields: { asUser: [785, 4], asideText: [679, 31], autoHide: [785, 4], blockResponse: [15349, 9], blocks: [3709, 637], createdAt: [19803, 13], forkable: [19759, 4], responseTimestamp: [7253, 13], sender: [19803, 10], text: [19278, 190], timestamp: [19803, 13], updatedAt: [19803, 13], references: [3330, 40], command: [94, 12], isCommand: [94, 4], compacted: [1689, 4], context: [4153, 49], status: [35, 4], trashedAt: [122, 13], asideContext: [6, 68] } },
  Artifact: { entities: 2033, fields: { artifactType: [2033, 8], content: [855, 771], createdAt: [2033, 13], title: [2033, 44], color: [50, 8], updatedAt: [156, 13] } },
  Thread: { entities: 343, fields: { createdAt: [343, 13], instructions: [343, 2], lastMessageTimestamp: [343, 13], shortCode: [343, 7], status: [343, 9], tags: [343, 15], timestamp: [343, 13], topic: [343, 40], updatedAt: [343, 13], pinned: [10, 4], lastVisitedTimestamp: [58, 13], chatState: [49, 7], context: [36, 1611], archived: [38, 4], role: [1, 19] } },
  Note: { entities: 280, fields: { completed: [280, 5], content: [202, 521], createdAt: [280, 13], displayOrder: [280, 1], icon: [280, 4], label: [280, 14], lastSeen: [280, 5], noteType: [280, 8], shortCode: [280, 10], title: [280, 16], updatedAt: [280, 13], favorite: [7, 4], savedDisplayOrder: [3, 1], trashedAt: [10, 13] } },
  Node: { entities: 137, fields: { actionId: [70, 30], createdAt: [137, 13], description: [137, 0], fieldMappings: [74, 184], final: [99, 0], label: [137, 15], nodeType: [137, 9], params: [69, 0], eventType: [35, 18], scope: [35, 8], trackKey: [35, 20], conditions: [21, 267], role: [5, 15], updatedAt: [2, 13], actionFn: [1, 2], mode: [1, 10], flowRef: [4, 26], propagateCtx: [4, 4] } },
  Terminal: { entities: 136, fields: { active: [136, 5], closedAt: [136, 13], cols: [136, 3], createdAt: [136, 13], cwd: [136, 42], pid: [136, 5], rows: [136, 2], shell: [136, 10], title: [136, 11], updatedAt: [136, 13], customTitle: [17, 11] } },
  Action: { entities: 69, fields: { category: [69, 10], createdAt: [69, 13], description: [69, 65], input: [69, 199], label: [69, 20], output: [69, 0], shortCode: [69, 8], sourceHash: [68, 18], updatedAt: [69, 13], actionFn: [25, 1177] } },
  Prompt: { entities: 6, fields: { category: [6, 11], createdAt: [6, 13], description: [6, 73], inputs: [6, 126], label: [6, 20], shortCode: [6, 10], sourceHash: [6, 18], templateFn: [4, 419], updatedAt: [6, 13] } },
  Flow: { entities: 5, fields: { createdAt: [5, 13], flowType: [5, 10], label: [5, 13], shortCode: [5, 11], sourceHash: [5, 18], role: [1, 13] } },
  BrowserTab: { entities: 2, fields: { createdAt: [2, 13], displayOrder: [2, 1], favicon: [2, 48], groupId: [2, 2], isMuted: [2, 5], title: [2, 51], updatedAt: [2, 13], url: [2, 129] } },
  Collection: { entities: 1, fields: { createdAt: [1, 13], displayOrder: [1, 4], name: [1, 10], sourceHash: [1, 18], updatedAt: [1, 13] } },
  Document: { entities: 1, fields: { createdAt: [1, 13], displayOrder: [1, 4], name: [1, 10], shortCode: [1, 7], sourceHash: [1, 18], tags: [1, 11], updatedAt: [1, 13] } },
  Settings: { entities: 1, fields: { createdAt: [1, 13], updatedAt: [1, 13] } },
};
export const LARGE: Record<string, [count: number, meanBytes: number]> = {
  'Message.blocks': [3770, 36762],
  'Artifact.content': [1178, 10445],
  'Message.text': [525, 5837],
  'Note.content': [78, 5837],
  'Action.actionFn': [44, 7373],
  'Thread.context': [14, 2662],
  'Prompt.templateFn': [2, 4506],
  'Settings.data': [1, 7475],
  'Document.content': [1, 2560],
};
export const RELATIONS: Record<string, number> = {'contains': 40019, 'transitions_to': 102, 'instance_of': 70, 'has': 2033, 'relates_to': 2033, 'references': 1, 'forked_from': 21, 'compacts': 1519};

/** Kinds the engine writes itself, or that would change what the data is (a role, the trash): not generated */
const ENGINE_OWNED = new Set(['role', 'createdAt', 'updatedAt', 'trashedAt', 'shortCode']);

/** A small value of about `bytes` serialized, of the kind such a field holds */
function small(bytes: number, n: number): unknown {
  if (bytes <= 2) return n % 10;
  if (bytes <= 5) return n % 2 === 0;
  if (bytes === 13) return 1_700_000_000_000 + n;
  return `${n}:`.padEnd(bytes - 2, 'x');
}

/** The id of a type's `i`th entity, as `loadProductionShape` names them */
export const productionId = (type: string, i: number) => `${type}-${i}` as EARS.EntityId;

/**
 * Loads the production shape into an engine's admin face, in one unit: every type's entities with their fields, each
 * large value in place of a small one for the first of its count, and the relations per kind. A thread `contains` its
 * messages, spread evenly, which is how the app reads them (`threadQueries.messages`); every other relation joins
 * entities chosen evenly across the whole set
 */
export function loadProductionShape(admin: EarsAdmin): void {
  admin.bulkLoad(() => {
    const all: EARS.EntityId[] = [];
    for (const [type, { entities, fields }] of Object.entries(TYPES)) {
      const large = Object.entries(LARGE).filter(([key]) => key.startsWith(`${type}.`)).map(([key, [n, size]]) => [key.slice(type.length + 1), n, size] as const);
      for (let i = 0; i < entities; i++) {
        const id = productionId(type, i);
        admin.bulkLoadAttr(id, 'createdAt' as EARS.AttrKind, 1_700_000_000_000 + i);
        for (const [kind, [n, bytes]] of Object.entries(fields)) {
          if (!ENGINE_OWNED.has(kind) && i < n) admin.bulkLoadAttr(id, kind as EARS.AttrKind, small(bytes, i));
        }
        for (const [kind, n, size] of large) if (i < n) admin.bulkLoadAttr(id, kind as EARS.AttrKind, `${i}:`.padEnd(size, 'y'));
        all.push(id);
      }
    }
    const threads = TYPES.Thread!.entities;
    let r = 0;
    const relate = (kind: string, source: EARS.EntityId, target: EARS.EntityId) =>
      admin.bulkLoadRelation(`Relation-${r++}` as EARS.RelationId, kind, source, target);
    for (let i = 0; i < TYPES.Message!.entities; i++) relate('contains', productionId('Thread', i % threads), productionId('Message', i));
    for (const [kind, n] of Object.entries(RELATIONS)) {
      const rest = kind === 'contains' ? n - TYPES.Message!.entities : n;
      for (let i = 0; i < rest; i++) relate(kind, all[(i * 31) % all.length]!, all[(i * 7 + 1) % all.length]!);
    }
  });
}
