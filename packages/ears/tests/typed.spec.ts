import { afterEach, describe, expect, it } from 'vitest';
import {
  asEntityId,
  createEarsEngine,
  defineEars,
  type EARS,
  type EarsSchemaShape,
  type Schema,
} from '../src/index.ts';
import { createTemporaryDatabases } from '../src/lmdb/index.ts';

interface WorkspaceSchema extends EarsSchemaShape {
  readonly entities: {
    readonly Project: { readonly name: string; readonly archived?: boolean };
    readonly Issue: { readonly title: string; readonly points: number; readonly projectId?: EARS.EntityId<'Project'> };
  };
  readonly relations: 'belongsTo' | 'dependsOn';
}

const runtimeSchema: Schema = {
  types: {
    Project: {
      fields: {
        name: { type: 'string' },
        archived: { type: 'boolean', optional: true },
      },
    },
    Issue: {
      fields: {
        title: { type: 'string' },
        points: { type: 'integer' },
        projectId: { type: 'ref', to: ['Project'], optional: true },
      },
    },
  },
  relations: {
    belongsTo: {
      cardinality: 'manyToOne',
      endpoints: [{ source: 'Issue', target: 'Project' }],
    },
    dependsOn: {
      cardinality: 'manyToMany',
      endpoints: [{ source: 'Issue', target: 'Issue' }],
    },
  },
};

const temps = createTemporaryDatabases();

afterEach(() => {
  temps.disposeAll();
});

describe('@elmdb/ears defineEars typed facade', () => {
  it('enforces entity shapes, branded IDs, and relation kinds end-to-end', () => {
    const { database } = temps.open({ schema: runtimeSchema });
    const engine = createEarsEngine({ database });
    const typed = defineEars<WorkspaceSchema>(() => engine);

    const projId: EARS.EntityId<'Project'> = typed.create('Project', { name: 'elmdb v2' }, 'proj-1');
    const issueId: EARS.EntityId<'Issue'> = typed.create(
      'Issue',
      { title: 'Rewrite EARS', points: 5, projectId: projId },
      'iss-1',
    );
    typed.link(issueId, 'belongsTo', projId);

    const row = typed.get(issueId);
    expect(row).not.toBeNull();
    const title: string = row!.fields.title;
    const points: number = row!.fields.points;
    expect(title).toBe('Rewrite EARS');
    expect(points).toBe(5);

    const queried = typed.query.of('Issue').where('points', { gte: 5 }).all();
    expect(queried).toHaveLength(1);
    expect(queried[0]?.fields.title).toBe('Rewrite EARS');

    typed.patch(issueId, { points: 8 });
    expect(typed.get(issueId)?.fields.points).toBe(8);

    const explicitId = asEntityId<'Project'>('proj-1');
    expect(typed.query.ids([explicitId]).first()?.fields.name).toBe('elmdb v2');

    typed.batch((b) => {
      const slot = b.create('Issue', { title: 'Docs', points: 2 }, 'iss-2');
      b.patch(slot, { points: 3 });
      b.link(slot, 'dependsOn', issueId);
    });
    expect(typed.get(asEntityId<'Issue'>('iss-2'))?.fields.points).toBe(3);
    expect(typed.getEdges(asEntityId<'Issue'>('iss-2'), { relKind: 'dependsOn', dir: 'out' })).toHaveLength(1);

    // P1-11: Distributive TypedEntityRow union narrowing on ofTypes
    const mixed = typed.query.ofTypes(['Project', 'Issue']).all();
    let seenIssuePoints = 0;
    for (const entry of mixed) {
      if (entry.typeName === 'Issue') {
        seenIssuePoints += entry.fields.points;
      } else {
        expect(typeof entry.fields.name).toBe('string');
      }
    }
    expect(seenIssuePoints).toBe(11);
    expect(typed.admin().stats().typeCounts['Issue']).toBe(2);
    expect(typed.watches().settled()).toBe(true);

    // Compile-time negative checks (verified by tsc during `npm run typecheck:ears`):
    const _typeChecks = (): void => {
      // @ts-expect-error raw unbranded string is rejected where EARS.EntityId is required
      typed.get('proj-1');
      // @ts-expect-error wrong branded entity id passed to typed.patch
      typed.patch(projId, { points: 1 });
      // @ts-expect-error unknown relation kind
      typed.link(issueId, 'nonexistentRel', projId);
      // @ts-expect-error unknown entity type
      typed.query.of('UnknownType');
      typed.batch((b) => {
        const pSlot = b.create('Project', { name: 'P' });
        // @ts-expect-error Issue field rejected on Project TypedBatchSlot
        b.unset(pSlot, ['points']);
      });
    };
    expect(typeof _typeChecks).toBe('function');
  });
});
