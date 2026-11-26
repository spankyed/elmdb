//! Unified `ProjectionRegistry` implementing `elmdb_engine::ProjectionSink<S, V>`.
//!
//! Dispatches coalesced per-entity & per-edge deltas by `FieldMask` / `liveness_changed`,
//! enforces watermark-bounded chunked builds (`key <= built_up_to || key > high_water`),
//! and drives unified `rebuild()`, `check()`, `index_status()`, and `stats()`.

use std::sync::Arc;

use elmdb_engine::{
    CompiledSchema, EdgeDelta, Engine, EntityDelta, EntityKey, IndexDef, IndexId, IndexKind, PackedRow, ProjectionSink,
    SchemaDiff, SystemDbs, max_type_key, read_packed_row, scan_type_keys, scan_type_keys_in_range,
};
use elmdb_storage::{Clock, DbFlags, GatedWriteTxn, KvCursor, KvRead, KvStore, SubDbHandle, Vfs};
use elmdb_wire::{ElmdbError, ErrorCode, Result};

use crate::order::{self, CollatorCache};
use crate::state::{
    BuildState, CheckReport, DEFAULT_BUILD_CHUNK, IndexMismatch, IndexStatus, KvMut, MemCheckTxn, ProjectionStats,
    delete_build_state, diff_subdb_against_expected, list_persisted_state_ids, read_build_state, write_build_state,
};
use crate::{rank, text, trigram, value};

/// Optional extension hook allowing sibling crates (`elmdb-vec`, Push PageRank) to register
/// directly into `ProjectionRegistry` under the exact same `BuildState` / `check()` / `rebuild()` lifecycle.
pub trait ExternalProjection<S: KvStore, V: Vfs>: Send + Sync {
    fn handles(&self, kind: &IndexKind) -> bool;

    fn apply_entity_delta(
        &self,
        txn: &mut GatedWriteTxn<'_, S, V>,
        schema: &CompiledSchema,
        dbs: &SystemDbs,
        def: &IndexDef,
        db: SubDbHandle,
        delta: &EntityDelta,
    ) -> Result<()>;

    fn apply_edge_delta(
        &self,
        txn: &mut GatedWriteTxn<'_, S, V>,
        schema: &CompiledSchema,
        dbs: &SystemDbs,
        def: &IndexDef,
        db: SubDbHandle,
        delta: &EdgeDelta,
    ) -> Result<()>;

    fn on_index_added(
        &self,
        txn: &mut GatedWriteTxn<'_, S, V>,
        schema: &CompiledSchema,
        dbs: &SystemDbs,
        def: &IndexDef,
        db: SubDbHandle,
    ) -> Result<BuildState>;

    fn rebuild_index(
        &self,
        txn: &mut GatedWriteTxn<'_, S, V>,
        schema: &CompiledSchema,
        dbs: &SystemDbs,
        def: &IndexDef,
        db: SubDbHandle,
    ) -> Result<()>;

    fn check_index(
        &self,
        txn: &S::RoTxn<'_>,
        schema: &CompiledSchema,
        dbs: &SystemDbs,
        def: &IndexDef,
        db: Option<SubDbHandle>,
    ) -> Result<Vec<IndexMismatch>>;
}

/// Controls whether newly added indexes over existing data initialize in watermark `Building` mode
/// (default — zero `set_schema` write-lock stall!) or build synchronously inside `set_schema`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BuildPolicy {
    #[default]
    DeferredWatermark,
    EagerInSchemaChange,
}

/// Single authoritative projection registry for elmdb v2.
pub struct ProjectionRegistry<S: KvStore, V: Vfs> {
    collators: Arc<CollatorCache>,
    build_policy: BuildPolicy,
    externals: Vec<Arc<dyn ExternalProjection<S, V>>>,
}

impl<S: KvStore, V: Vfs> Default for ProjectionRegistry<S, V> {
    fn default() -> Self {
        Self::new(BuildPolicy::DeferredWatermark)
    }
}

impl<S: KvStore, V: Vfs> ProjectionRegistry<S, V> {
    pub fn new(build_policy: BuildPolicy) -> Self {
        Self { collators: Arc::new(CollatorCache::default()), build_policy, externals: Vec::new() }
    }

    pub fn with_external(mut self, ext: Arc<dyn ExternalProjection<S, V>>) -> Self {
        self.externals.push(ext);
        self
    }

    pub fn collators(&self) -> &Arc<CollatorCache> {
        &self.collators
    }

    fn find_external(&self, kind: &IndexKind) -> Option<&Arc<dyn ExternalProjection<S, V>>> {
        self.externals.iter().find(|e| e.handles(kind))
    }

    fn live_view(row: Option<&PackedRow>) -> Option<&PackedRow> {
        row.filter(|r| !r.header.is_trashed())
    }

    /// Applies one entity's `(before, after)` live-row delta to a built-in projection.
    fn apply_builtin_delta<W: KvMut>(
        &self,
        txn: &mut W,
        dbs: &SystemDbs,
        def: &IndexDef,
        db: SubDbHandle,
        key: EntityKey,
        pair: (Option<&PackedRow>, Option<&PackedRow>),
    ) -> Result<()> {
        let live_before = Self::live_view(pair.0);
        let live_after = Self::live_view(pair.1);
        if live_before.is_none() && live_after.is_none() {
            return Ok(());
        }
        match &def.kind {
            IndexKind::Value { fields, unique } => value::apply_delta(
                txn,
                dbs,
                value::ValueIndexSpec { db, index_name: &def.name, fields, unique: *unique },
                key,
                live_before,
                live_after,
            ),
            IndexKind::Order { field, locale } => {
                let cached = self.collators.get_or_build(locale)?;
                order::apply_delta(txn, db, &cached, *field, key, live_before, live_after)
            }
            IndexKind::Text { fields } => text::apply_delta(txn, db, fields, None, key, live_before, live_after),
            IndexKind::Trigram { fields } => trigram::apply_delta(txn, db, fields, key, live_before, live_after),
            IndexKind::Vector { .. } | IndexKind::Rank { .. } => Ok(()),
        }
    }

    /// Advances watermark-bounded builds by up to `chunk_size` entities across the first `Building` index.
    /// Returns `true` if work was performed (caller can loop until `false` to finish all builds).
    pub fn drive_build_chunk<C: Clock>(&self, engine: &Engine<S, V, C>, chunk_size: usize) -> Result<bool> {
        // Fast lock-free RO pre-check so idle calls never acquire the write lock or fsync!
        {
            let (ro, schema) = engine.begin_read_pinned()?;
            let dbs = engine.dbs();
            let mut has_building = false;
            for def in &schema.indexes {
                if matches!(read_build_state(&ro, &dbs, def.id)?, Some(BuildState::Building { .. })) {
                    has_building = true;
                    break;
                }
            }
            if !has_building {
                return Ok(false);
            }
        }
        let chunk = chunk_size.max(1);
        let (did_work, _) = engine.run_maintenance_write(|txn, schema, dbs| {
            for def in &schema.indexes {
                let Some(state) = read_build_state(txn, dbs, def.id)? else {
                    continue;
                };
                let BuildState::Building { built_up_to, high_water } = state else {
                    continue;
                };
                if built_up_to >= high_water {
                    write_build_state(txn, dbs, def.id, &BuildState::Ready)?;
                    return Ok(true);
                }

                let db = txn.open_subdb(&def.subdb_slot().name(), DbFlags::DEFAULT)?;
                let after = if built_up_to.0 == 0 { None } else { Some(built_up_to) };
                let batch_keys = scan_type_keys_in_range(txn, dbs, def.type_id, after, Some(high_water), chunk)?;
                if batch_keys.is_empty() {
                    write_build_state(txn, dbs, def.id, &BuildState::Ready)?;
                    return Ok(true);
                }

                let last_key = *batch_keys.last().expect("non-empty");
                // Run the chunk inside a savepoint so a UniqueViolation rolls back cleanly and transitions
                // the index to `BuildState::Failed` rather than corrupting partial state!
                let chunk_res: Result<()> = txn.with_savepoint(|sp| {
                    for &ek in &batch_keys {
                        if let Some(row) = read_packed_row(sp, dbs, ek)? {
                            if row.header.is_trashed() {
                                continue;
                            }
                            if let Some(ext) = self.find_external(&def.kind) {
                                let delta = EntityDelta {
                                    key: ek,
                                    type_id: def.type_id,
                                    before: None,
                                    after: Some(row),
                                    changed_fields: def.field_mask(),
                                    liveness_changed: true,
                                };
                                ext.apply_entity_delta(sp, schema, dbs, def, db, &delta)?;
                            } else {
                                self.apply_builtin_delta(sp, dbs, def, db, ek, (None, Some(&row)))?;
                            }
                        }
                    }
                    Ok(())
                });

                match chunk_res {
                    Ok(()) => {
                        let next_state = if batch_keys.len() < chunk || last_key >= high_water {
                            BuildState::Ready
                        } else {
                            BuildState::Building { built_up_to: last_key, high_water }
                        };
                        write_build_state(txn, dbs, def.id, &next_state)?;
                    }
                    Err(err) => {
                        write_build_state(
                            txn,
                            dbs,
                            def.id,
                            &BuildState::Failed { code: err.code, message: err.message },
                        )?;
                    }
                }
                return Ok(true);
            }
            Ok(false)
        })?;
        Ok(did_work)
    }

    /// Drives all watermark-bounded background builds to completion (chunked by `DEFAULT_BUILD_CHUNK`).
    pub fn drain_all_builds<C: Clock>(&self, engine: &Engine<S, V, C>) -> Result<usize> {
        let mut chunks = 0usize;
        while self.drive_build_chunk(engine, DEFAULT_BUILD_CHUNK)? {
            chunks += 1;
        }
        Ok(chunks)
    }

    /// Rebuilds one index (or all indexes when `target == None`) from scratch from primary storage
    /// using O(1) `truncate` on the pooled `SubDbSlot` (zero DBI leak!).
    pub fn rebuild<C: Clock>(&self, engine: &Engine<S, V, C>, target: Option<IndexId>) -> Result<usize> {
        let (rebuilt, _) = engine.run_maintenance_write(|txn, schema, dbs| {
            let mut count = 0usize;
            for def in &schema.indexes {
                if let Some(tid) = target
                    && def.id != tid
                {
                    continue;
                }
                let db = txn.open_subdb(&def.subdb_slot().name(), DbFlags::DEFAULT)?;
                txn.truncate(db)?;

                if let Some(ext) = self.find_external(&def.kind) {
                    ext.rebuild_index(txn, schema, dbs, def, db)?;
                } else if let IndexKind::Rank { relations, weighted } = &def.kind {
                    rank::rebuild_in_txn(txn, dbs, db, def.type_id, relations, *weighted, true)?;
                } else {
                    if let IndexKind::Order { locale, .. } = &def.kind {
                        let cached = self.collators.get_or_build(locale)?;
                        order::write_stamp(txn, db, &cached)?;
                    }
                    let keys = scan_type_keys(txn, dbs, def.type_id)?;
                    for ek in keys {
                        if let Some(row) = read_packed_row(txn, dbs, ek)? {
                            self.apply_builtin_delta(txn, dbs, def, db, ek, (None, Some(&row)))?;
                        }
                    }
                }
                write_build_state(txn, dbs, def.id, &BuildState::Ready)?;
                count += 1;
            }
            if let Some(tid) = target
                && count == 0
            {
                return Err(ElmdbError::new(ErrorCode::UnknownIndex, format!("unknown index id {}", tid.0)));
            }
            Ok(count)
        })?;
        Ok(rebuilt)
    }

    /// Differential verifier proving `derived == rebuild(primary)` byte-for-byte for every Ready index!
    pub fn check<C: Clock>(&self, engine: &Engine<S, V, C>, target: Option<IndexId>) -> Result<CheckReport> {
        let (txn, schema) = engine.begin_read_pinned()?;
        let dbs = engine.dbs();
        let mut report = CheckReport::default();

        let mut found_target = false;
        for def in &schema.indexes {
            if let Some(tid) = target
                && def.id != tid
            {
                continue;
            }
            found_target = true;
            let state = read_build_state(&txn, &dbs, def.id)?.unwrap_or(BuildState::Ready);
            if !state.is_ready() {
                report.mismatches.push(IndexMismatch {
                    index_id: def.id,
                    index_name: def.name.clone(),
                    detail: format!("index not ready: {state:?}"),
                });
                report.checked_indexes += 1;
                continue;
            }

            let actual_db = engine.gate().open_subdb_ro(&txn, &def.subdb_slot().name(), DbFlags::DEFAULT)?;
            if let Some(ext) = self.find_external(&def.kind) {
                let mut m = ext.check_index(&txn, &schema, &dbs, def, actual_db)?;
                report.mismatches.append(&mut m);
                report.checked_indexes += 1;
                continue;
            }
            if let IndexKind::Rank { relations, weighted } = &def.kind {
                let spec = rank::RankSpec { type_id: def.type_id, relations, weighted: *weighted };
                let mut m = rank::check_rank(&txn, &dbs, actual_db, def.id, &def.name, spec)?;
                report.mismatches.append(&mut m);
                report.checked_indexes += 1;
                continue;
            }
            if matches!(def.kind, IndexKind::Vector { .. }) {
                continue;
            }

            // Rebuild into an isolated in-memory overlay using the exact same projection apply code.
            let synth_handle = SubDbHandle::from_raw(u32::MAX);
            let mut overlay = MemCheckTxn::new(&txn, synth_handle);
            if let IndexKind::Order { locale, .. } = &def.kind {
                let cached = self.collators.get_or_build(locale)?;
                order::write_stamp(&mut overlay, synth_handle, &cached)?;
            }
            let keys = scan_type_keys(&txn, &dbs, def.type_id)?;
            for ek in keys {
                if let Some(row) = read_packed_row(&txn, &dbs, ek)? {
                    self.apply_builtin_delta(&mut overlay, &dbs, def, synth_handle, ek, (None, Some(&row)))?;
                }
            }
            let expected = overlay.into_map();
            let mut m = diff_subdb_against_expected(&txn, actual_db, &expected, def.id, &def.name, 16)?;
            report.mismatches.append(&mut m);
            report.checked_indexes += 1;
        }
        if let Some(tid) = target
            && !found_target
        {
            return Err(ElmdbError::new(ErrorCode::UnknownIndex, format!("unknown index id {}", tid.0)));
        }
        Ok(report)
    }

    /// Returns the `BuildState` status of every declared index.
    pub fn index_status<C: Clock>(&self, engine: &Engine<S, V, C>) -> Result<Vec<IndexStatus>> {
        let (txn, schema) = engine.begin_read_pinned()?;
        let dbs = engine.dbs();
        let mut out = Vec::with_capacity(schema.indexes.len());
        for def in &schema.indexes {
            let state = read_build_state(&txn, &dbs, def.id)?.unwrap_or(BuildState::Ready);
            out.push(IndexStatus { index_id: def.id, name: def.name.clone(), slot: def.slot, state });
        }
        Ok(out)
    }

    /// Streaming sub-DB key/value byte statistics across every declared index (O(1) RAM!).
    pub fn stats<C: Clock>(&self, engine: &Engine<S, V, C>) -> Result<Vec<ProjectionStats>> {
        let (txn, schema) = engine.begin_read_pinned()?;
        let mut out = Vec::with_capacity(schema.indexes.len());
        for def in &schema.indexes {
            let mut entry_count = 0u64;
            let mut key_bytes = 0u64;
            let mut value_bytes = 0u64;
            if let Some(db) = engine.gate().open_subdb_ro(&txn, &def.subdb_slot().name(), DbFlags::DEFAULT)? {
                let mut cur = txn.cursor(db)?;
                let mut step = cur.first()?;
                while let Some((k, v)) = step {
                    entry_count += 1;
                    key_bytes += k.len() as u64;
                    value_bytes += v.len() as u64;
                    step = cur.next()?;
                }
            }
            out.push(ProjectionStats {
                index_id: def.id,
                name: def.name.clone(),
                slot: def.slot,
                entry_count,
                key_bytes,
                value_bytes,
            });
        }
        Ok(out)
    }
}

impl<S: KvStore, V: Vfs> ProjectionSink<S, V> for ProjectionRegistry<S, V> {
    fn apply_deltas(
        &self,
        txn: &mut GatedWriteTxn<'_, S, V>,
        schema: &CompiledSchema,
        dbs: &SystemDbs,
        entity_deltas: &[EntityDelta],
        edge_deltas: &[EdgeDelta],
    ) -> Result<()> {
        if entity_deltas.is_empty() && edge_deltas.is_empty() {
            return Ok(());
        }

        // Dispatch coalesced entity deltas via `TypeId` + `FieldMask` / `liveness_changed`.
        for def in &schema.indexes {
            if let IndexKind::Rank { relations, weighted } = &def.kind {
                let state = read_build_state(txn, dbs, def.id)?.unwrap_or(BuildState::Ready);
                if state.is_ready() {
                    let db = txn.open_subdb(&def.subdb_slot().name(), DbFlags::DEFAULT)?;
                    let spec = rank::RankSpec { type_id: def.type_id, relations, weighted: *weighted };
                    rank::apply_deltas(txn, dbs, db, spec, entity_deltas, edge_deltas)?;
                }
                continue;
            }
            let mask = def.field_mask();
            let mut state_opt: Option<BuildState> = None;

            // Collect matching deltas for this index so unique Value indexes can run two-pass
            // (all removals before all insertions), making intra-batch swaps and destroy+restore order-independent!
            let mut matched: Vec<&EntityDelta> = Vec::new();
            for delta in entity_deltas {
                if delta.type_id != def.type_id {
                    continue;
                }
                if !delta.liveness_changed && !mask.intersects(&delta.changed_fields) {
                    continue;
                }
                let state = match &state_opt {
                    Some(s) => s,
                    None => {
                        let loaded = read_build_state(txn, dbs, def.id)?.unwrap_or(BuildState::Ready);
                        state_opt = Some(loaded);
                        state_opt.as_ref().expect("set")
                    }
                };
                if !state.should_apply_foreground(delta.key) {
                    continue;
                }
                matched.push(delta);
            }
            if matched.is_empty() {
                continue;
            }
            let db = txn.open_subdb(&def.subdb_slot().name(), DbFlags::DEFAULT)?;

            if let IndexKind::Value { fields, unique: true } = &def.kind {
                let spec = value::ValueIndexSpec { db, index_name: &def.name, fields, unique: true };
                for delta in &matched {
                    value::remove_row(txn, spec, delta.key, Self::live_view(delta.before.as_ref()))?;
                }
                for delta in &matched {
                    value::insert_row(txn, dbs, spec, delta.key, Self::live_view(delta.after.as_ref()))?;
                }
                continue;
            }

            for delta in matched {
                if let Some(ext) = self.find_external(&def.kind) {
                    ext.apply_entity_delta(txn, schema, dbs, def, db, delta)?;
                } else {
                    self.apply_builtin_delta(
                        txn,
                        dbs,
                        def,
                        db,
                        delta.key,
                        (delta.before.as_ref(), delta.after.as_ref()),
                    )?;
                }
            }
        }

        // Dispatch edge deltas to any external edge-driven projections (e.g. Push PageRank).
        if !edge_deltas.is_empty() {
            for def in &schema.indexes {
                if let Some(ext) = self.find_external(&def.kind) {
                    let state = read_build_state(txn, dbs, def.id)?.unwrap_or(BuildState::Ready);
                    if !state.is_ready() {
                        continue;
                    }
                    let db = txn.open_subdb(&def.subdb_slot().name(), DbFlags::DEFAULT)?;
                    for ed in edge_deltas {
                        ext.apply_edge_delta(txn, schema, dbs, def, db, ed)?;
                    }
                }
            }
        }
        Ok(())
    }

    fn on_schema_change(
        &self,
        txn: &mut GatedWriteTxn<'_, S, V>,
        schema: &CompiledSchema,
        dbs: &SystemDbs,
        diff: &SchemaDiff,
    ) -> Result<()> {
        // Remove persisted build states for dropped indexes.
        let persisted = list_persisted_state_ids(txn, dbs)?;
        for id in persisted {
            if schema.index_by_id(id).is_none() {
                delete_build_state(txn, dbs, id)?;
            }
        }

        // Initialize newly added indexes.
        for &idx_id in &diff.added_indexes {
            let Some(def) = schema.index_by_id(idx_id) else {
                continue;
            };
            let db = txn.open_subdb(&def.subdb_slot().name(), DbFlags::DEFAULT)?;
            if let Some(ext) = self.find_external(&def.kind) {
                let state = ext.on_index_added(txn, schema, dbs, def, db)?;
                write_build_state(txn, dbs, def.id, &state)?;
                continue;
            }
            if let IndexKind::Rank { relations, weighted } = &def.kind {
                let converge_now = self.build_policy == BuildPolicy::EagerInSchemaChange;
                rank::rebuild_in_txn(txn, dbs, db, def.type_id, relations, *weighted, converge_now)?;
                write_build_state(txn, dbs, def.id, &BuildState::Ready)?;
                continue;
            }
            if let IndexKind::Order { locale, .. } = &def.kind {
                let cached = self.collators.get_or_build(locale)?;
                order::write_stamp(txn, db, &cached)?;
            }
            let max_key = max_type_key(txn, dbs, def.type_id)?;
            match (max_key, self.build_policy) {
                (None, _) => {
                    write_build_state(txn, dbs, def.id, &BuildState::Ready)?;
                }
                (Some(hw), BuildPolicy::DeferredWatermark) => {
                    write_build_state(
                        txn,
                        dbs,
                        def.id,
                        &BuildState::Building { built_up_to: EntityKey(0), high_water: hw },
                    )?;
                }
                (Some(_), BuildPolicy::EagerInSchemaChange) => {
                    let keys = scan_type_keys(txn, dbs, def.type_id)?;
                    for ek in keys {
                        if let Some(row) = read_packed_row(txn, dbs, ek)? {
                            self.apply_builtin_delta(txn, dbs, def, db, ek, (None, Some(&row)))?;
                        }
                    }
                    write_build_state(txn, dbs, def.id, &BuildState::Ready)?;
                }
            }
        }
        Ok(())
    }
}
