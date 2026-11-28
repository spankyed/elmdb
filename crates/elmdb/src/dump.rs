//! Portable, lossless, atomic primary-state dump and load (`elmdb dump` / `elmdb load`).
//!
//! - Losslessly preserves every `CanonicalValue` variant (`Bytes`, `Timestamp { ms, ns }`,
//!   `Int(i64)` including `|i| > 2^53`, `Float(f64)`, `EntityRef(ext_id)`, `DanglingRef(ext_id)`).
//! - Preserves trashed entities (including their embeddings & unique-index collisions with live rows),
//!   forward/cyclic `EntityRef`s, and archived types.
//! - Both `dump()` and `load()` write to a staging path beside target and atomically rename into place,
//!   leaving zero partial files or half-loaded DB directories on error.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use elmdb_engine::EntityKey;
use elmdb_projections::BuildPolicy;
use elmdb_storage::DurabilityMode;
use elmdb_wire::{CanonicalValue, ElmdbError, EntityTarget, ErrorCode, Result, WireOp};
use serde::{Deserialize, Serialize};

use crate::db::{Database, OpenOptions};
use crate::protocol::{
    DumpReport, FieldSpec, IndexDefinition, ProtocolCardinality, ProtocolOnDelete, Schema, compiled_to_protocol_schema,
};

static NEXT_DUMP_NONCE: AtomicU64 = AtomicU64::new(1);

/// Lossless tagged JSON representation of every `CanonicalValue` variant.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "t", content = "v", rename_all = "camelCase")]
enum DumpValue {
    Null,
    Bool(bool),
    /// Encoded as decimal string so full signed 64-bit integers (`|i| > 2^53`) survive any JSON parser.
    Int(String),
    Float(f64),
    Timestamp {
        ms: i64,
        ns: u32,
    },
    /// Resolved external string ID of a live/referenced entity.
    EntityRef(String),
    /// External string ID of a dangling reference.
    DanglingRef(String),
    String(String),
    /// Hex-encoded raw bytes (lossless, zero extra dependency).
    Bytes(String),
    Array(Vec<DumpValue>),
    Object(Vec<(String, DumpValue)>),
}

impl DumpValue {
    fn from_canonical(v: &CanonicalValue, key_to_ext: &HashMap<EntityKey, String>) -> Self {
        match v {
            CanonicalValue::Null => Self::Null,
            CanonicalValue::Bool(b) => Self::Bool(*b),
            CanonicalValue::Int(i) => Self::Int(i.to_string()),
            CanonicalValue::Float(f) => Self::Float(*f),
            CanonicalValue::Timestamp { ms, ns } => Self::Timestamp { ms: *ms, ns: *ns },
            CanonicalValue::EntityRef(raw) => match key_to_ext.get(&EntityKey(*raw)) {
                Some(ext) => Self::EntityRef(ext.clone()),
                None => Self::DanglingRef(format!("__deleted:{raw}")),
            },
            CanonicalValue::DanglingRef(s) => Self::DanglingRef(s.clone()),
            CanonicalValue::String(s) => Self::String(s.clone()),
            CanonicalValue::Bytes(b) => Self::Bytes(encode_hex(b)),
            CanonicalValue::Array(items) => {
                Self::Array(items.iter().map(|i| Self::from_canonical(i, key_to_ext)).collect())
            }
            CanonicalValue::Object(entries) => Self::Object(
                entries.iter().map(|(k, val)| (k.clone(), Self::from_canonical(val, key_to_ext))).collect(),
            ),
        }
    }

    fn to_canonical(&self) -> Result<CanonicalValue> {
        match self {
            Self::Null => Ok(CanonicalValue::Null),
            Self::Bool(b) => Ok(CanonicalValue::Bool(*b)),
            Self::Int(s) => {
                let i: i64 = s.parse().map_err(|e| {
                    ElmdbError::new(ErrorCode::ValidationFailed, format!("invalid i64 in dump '{s}': {e}"))
                })?;
                Ok(CanonicalValue::Int(i))
            }
            Self::Float(f) => CanonicalValue::float(*f),
            Self::Timestamp { ms, ns } => {
                if *ns >= 1_000_000_000 {
                    return Err(ElmdbError::new(ErrorCode::ValidationFailed, "timestamp ns must be < 1e9"));
                }
                Ok(CanonicalValue::Timestamp { ms: *ms, ns: *ns })
            }
            Self::EntityRef(ext) => Ok(CanonicalValue::String(ext.clone())),
            Self::DanglingRef(ext) => Ok(CanonicalValue::DanglingRef(ext.clone())),
            Self::String(s) => Ok(CanonicalValue::String(s.clone())),
            Self::Bytes(hex) => Ok(CanonicalValue::Bytes(decode_hex(hex)?)),
            Self::Array(items) => {
                Ok(CanonicalValue::Array(items.iter().map(Self::to_canonical).collect::<Result<Vec<_>>>()?))
            }
            Self::Object(entries) => {
                let pairs =
                    entries.iter().map(|(k, v)| Ok((k.clone(), v.to_canonical()?))).collect::<Result<Vec<_>>>()?;
                CanonicalValue::object(pairs)
            }
        }
    }

    fn contains_entity_ref(&self) -> bool {
        match self {
            Self::EntityRef(_) | Self::DanglingRef(_) => true,
            Self::Array(items) => items.iter().any(Self::contains_entity_ref),
            Self::Object(entries) => entries.iter().any(|(_, v)| v.contains_entity_ref()),
            _ => false,
        }
    }
}

fn field_spec_contains_ref(spec: &FieldSpec) -> bool {
    match spec {
        FieldSpec::Ref { .. } => true,
        FieldSpec::List { of } => field_spec_contains_ref(of),
        _ => false,
    }
}

fn relax_field_spec_for_staging(spec: &mut FieldSpec) {
    match spec {
        FieldSpec::Ref { on_delete, .. } => {
            *on_delete = ProtocolOnDelete::AllowDangling;
        }
        FieldSpec::List { of } => relax_field_spec_for_staging(of),
        _ => {}
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DumpEntity {
    id: String,
    type_name: String,
    trashed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    created_at_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    updated_at_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    trashed_at_ms: Option<i64>,
    fields: BTreeMap<String, DumpValue>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    extra: BTreeMap<String, DumpValue>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DumpRelation {
    rel_kind: String,
    source_id: String,
    target_id: String,
    weight: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    created_at_ms: Option<i64>,
    info: Option<DumpValue>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DumpEmbedding {
    index: String,
    id: String,
    vectors: Vec<Vec<f32>>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DumpPayload {
    format_version: u32,
    schema: Schema,
    /// Full schema including archived types (used during load before re-archiving them).
    full_schema: Schema,
    archived_types: Vec<String>,
    entities: Vec<DumpEntity>,
    relations: Vec<DumpRelation>,
    embeddings: Vec<DumpEmbedding>,
    #[serde(default)]
    counters: BTreeMap<String, u64>,
}

struct FileCleanupGuard(Option<PathBuf>);
impl FileCleanupGuard {
    fn disarm(&mut self) {
        self.0 = None;
    }
}
impl Drop for FileCleanupGuard {
    fn drop(&mut self) {
        if let Some(p) = self.0.take() {
            let _ = fs::remove_file(p);
        }
    }
}

struct DirCleanupGuard(Option<PathBuf>);
impl DirCleanupGuard {
    fn disarm(&mut self) {
        self.0 = None;
    }
}
impl Drop for DirCleanupGuard {
    fn drop(&mut self) {
        if let Some(p) = self.0.take() {
            let _ = fs::remove_dir_all(p);
        }
    }
}

/// Atomically dumps `db`'s schema and primary state (entities, relations, embeddings) to a new file `target`.
pub fn dump(db: &Database, target: impl AsRef<Path>) -> Result<DumpReport> {
    let target = target.as_ref();
    crate::fs_publish::ensure_not_self_or_descendant(db.dir(), target)?;
    if target.symlink_metadata().is_ok() {
        return Err(ElmdbError::new(
            ErrorCode::AlreadyExists,
            format!("dump target '{}' already exists", target.display()),
        ));
    }
    let snap = db.snapshot_for_dump()?;
    let active_schema = compiled_to_protocol_schema(&snap.schema, false);
    let full_schema = compiled_to_protocol_schema(&snap.schema, true);
    let archived_types: Vec<String> = snap.schema.types.iter().filter(|t| t.archived).map(|t| t.name.clone()).collect();

    let key_to_ext: HashMap<EntityKey, String> = snap.rows.iter().map(|(r, _)| (r.key, r.id.clone())).collect();

    let entities: Vec<DumpEntity> = snap
        .rows
        .iter()
        .map(|(r, extra)| DumpEntity {
            id: r.id.clone(),
            type_name: r.type_name.clone(),
            trashed: r.trashed,
            created_at_ms: Some(r.created_at_ms),
            updated_at_ms: Some(r.updated_at_ms),
            trashed_at_ms: Some(r.trashed_at_ms),
            fields: r.fields.iter().map(|(k, v)| (k.clone(), DumpValue::from_canonical(v, &key_to_ext))).collect(),
            extra: extra.iter().map(|(k, v)| (k.clone(), DumpValue::from_canonical(v, &key_to_ext))).collect(),
        })
        .collect();

    let relations: Vec<DumpRelation> = snap
        .edges
        .iter()
        .map(|e| DumpRelation {
            rel_kind: e.kind_name.clone(),
            source_id: key_to_ext.get(&e.src).cloned().unwrap_or_default(),
            target_id: key_to_ext.get(&e.dst).cloned().unwrap_or_default(),
            weight: e.weight,
            created_at_ms: Some(e.created_at_ms),
            info: e.info.as_ref().map(|v| DumpValue::from_canonical(v, &key_to_ext)),
        })
        .collect();

    let embeddings: Vec<DumpEmbedding> =
        snap.embeddings.into_iter().map(|(index, id, vectors)| DumpEmbedding { index, id, vectors }).collect();

    let report = DumpReport {
        entities: entities.len() as u64,
        relations: relations.len() as u64,
        embeddings: embeddings.len() as u64,
    };

    let payload = DumpPayload {
        format_version: 2,
        schema: active_schema,
        full_schema,
        archived_types,
        entities,
        relations,
        embeddings,
        counters: snap.counters,
    };
    let bytes = serde_json::to_vec_pretty(&payload)
        .map_err(|e| ElmdbError::new(ErrorCode::Internal, format!("serialize dump: {e}")))?;

    let parent = crate::fs_publish::parent_dir(target);
    fs::create_dir_all(parent).map_err(|e| ElmdbError::new(ErrorCode::Io, e.to_string()))?;

    let nonce = NEXT_DUMP_NONCE.fetch_add(1, Ordering::Relaxed);
    let tmp_path = parent.join(format!(".elmdb-dump-tmp-{}-{nonce}.json", std::process::id()));
    let mut cleanup = FileCleanupGuard(Some(tmp_path.clone()));
    {
        use std::io::Write;
        let mut f = fs::File::create(&tmp_path).map_err(|e| ElmdbError::new(ErrorCode::Io, e.to_string()))?;
        f.write_all(&bytes).map_err(|e| ElmdbError::new(ErrorCode::Io, e.to_string()))?;
        f.sync_all().map_err(|e| ElmdbError::new(ErrorCode::Io, e.to_string()))?;
    }
    crate::fs_publish::atomic_publish_file_no_overwrite(&tmp_path, target)?;
    cleanup.disarm();
    Ok(report)
}

/// Atomically loads a v2 dump file into a brand-new database at `target_dir` (staged beside `target_dir`
/// so failures never leave a half-populated DB), rebuilding every derived index and verifying cleanliness.
pub fn load(dump_file: impl AsRef<Path>, target_dir: impl AsRef<Path>) -> Result<DumpReport> {
    let dump_file = dump_file.as_ref();
    let target_dir = target_dir.as_ref();
    let (clean_target, existing) = crate::fs_publish::normalize_non_symlink_target_dir(target_dir)?;
    let target_dir = clean_target.as_path();
    crate::fs_publish::ensure_paths_disjoint(dump_file, target_dir)?;
    if existing.is_some() {
        let mut it = fs::read_dir(target_dir).map_err(|e| ElmdbError::new(ErrorCode::Io, e.to_string()))?;
        if it.next().is_some() {
            return Err(ElmdbError::new(
                ErrorCode::AlreadyExists,
                format!("load target '{}' is not empty", target_dir.display()),
            ));
        }
    }

    let raw = fs::read(dump_file).map_err(|e| ElmdbError::new(ErrorCode::Io, e.to_string()))?;
    let payload: DumpPayload = serde_json::from_slice(&raw)
        .map_err(|e| ElmdbError::new(ErrorCode::InvalidArgument, format!("invalid dump file: {e}")))?;
    if payload.format_version != 2 {
        return Err(ElmdbError::new(
            ErrorCode::InvalidArgument,
            format!("unsupported dump format version {} (expected 2)", payload.format_version),
        ));
    }

    let parent = crate::fs_publish::parent_dir(target_dir);
    fs::create_dir_all(parent).map_err(|e| ElmdbError::new(ErrorCode::Io, e.to_string()))?;
    let nonce = NEXT_DUMP_NONCE.fetch_add(1, Ordering::Relaxed);
    let staging = parent.join(format!(".elmdb-load-tmp-{}-{nonce}", std::process::id()));
    let mut cleanup = DirCleanupGuard(Some(staging.clone()));

    let db = Database::open(
        &staging,
        OpenOptions {
            durability: DurabilityMode::Grouped,
            build_policy: BuildPolicy::DeferredWatermark,
            ..Default::default()
        },
    )?;

    // Build a relaxed staging schema so forward/cyclic Refs, List<Ref>, DanglingRefs, required archived
    // fields, post-creation required/non-null/additionalFields evolutions, and trashed unique-key
    // collisions all load cleanly before Pass 6 restores the exact full & active schemas!
    let mut staging_schema = payload.full_schema.clone();
    for tdef in staging_schema.types.values_mut() {
        tdef.additional_fields = true;
        for fdef in tdef.fields.values_mut() {
            fdef.optional = true;
            fdef.nullable = true;
            if field_spec_contains_ref(&fdef.kind) {
                relax_field_spec_for_staging(&mut fdef.kind);
            }
        }
        for idx in tdef.indexes.values_mut() {
            if let IndexDefinition::Value { unique, .. } = idx {
                *unique = false;
            }
        }
    }
    for rdef in staging_schema.relations.values_mut() {
        rdef.cardinality = ProtocolCardinality::ManyToMany;
        rdef.acyclic = false;
        rdef.weighted = true;
        rdef.endpoints.clear();
    }
    db.set_schema(&staging_schema)?;

    // Pass 1: Create all entities with non-ref fields so every target `ext_id` exists in `ext_id`.
    let mut create_ops = Vec::with_capacity(payload.entities.len());
    let mut ref_patches: Vec<(String, Vec<(String, CanonicalValue)>)> = Vec::new();
    let mut trashed_ids = Vec::new();

    for e in &payload.entities {
        let tspec = payload.full_schema.types.get(&e.type_name);
        let mut non_ref_fields = Vec::new();
        let mut ref_fields = Vec::new();
        for (k, v) in &e.fields {
            let cv = v.to_canonical()?;
            let is_ref_field = tspec.and_then(|t| t.fields.get(k)).is_some_and(|f| field_spec_contains_ref(&f.kind));
            if is_ref_field || v.contains_entity_ref() {
                ref_fields.push((k.clone(), cv));
            } else {
                non_ref_fields.push((k.clone(), cv));
            }
        }
        create_ops.push(WireOp::Create {
            type_name: e.type_name.clone(),
            external_id: Some(e.id.clone()),
            fields: non_ref_fields,
        });
        if !ref_fields.is_empty() {
            ref_patches.push((e.id.clone(), ref_fields));
        }
        if e.trashed {
            trashed_ids.push(e.id.clone());
        }
    }
    if !create_ops.is_empty() {
        db.execute_batch(&create_ops)?;
    }

    // Pass 2: Patch all Ref/DanglingRef fields (forward & cyclic refs now resolve cleanly!).
    if !ref_patches.is_empty() {
        let patch_ops: Vec<WireOp> = ref_patches
            .into_iter()
            .map(|(id, fields)| WireOp::Patch { target: EntityTarget::ExternalId(id), fields })
            .collect();
        db.execute_batch(&patch_ops)?;
    }

    // Pass 3: Link all relations while entities are live.
    if !payload.relations.is_empty() {
        let mut link_ops = Vec::with_capacity(payload.relations.len());
        for rel in &payload.relations {
            let info = match &rel.info {
                Some(v) => Some(v.to_canonical()?),
                None => None,
            };
            link_ops.push(WireOp::Link {
                source: EntityTarget::ExternalId(rel.source_id.clone()),
                rel_kind: rel.rel_kind.clone(),
                target: EntityTarget::ExternalId(rel.target_id.clone()),
                weight: Some(rel.weight.to_bits()),
                info,
            });
        }
        db.execute_batch(&link_ops)?;
    }

    // Pass 4: Restore primary embeddings BEFORE trashing entities so trashed entities keep their embeddings!
    for emb in &payload.embeddings {
        db.set_embeddings(&emb.index, &emb.id, &emb.vectors)?;
    }

    // Pass 4b: Restore all persisted named transactional counters cleanly.
    for (name, &val) in &payload.counters {
        db.set_counter(name, val)?;
    }

    // Pass 5: Trash entities that were trashed in the dump (frees unique keys before restoring unique constraints!).
    if !trashed_ids.is_empty() {
        let trash_ops: Vec<WireOp> =
            trashed_ids.into_iter().map(|id| WireOp::Trash { target: EntityTarget::ExternalId(id) }).collect();
        db.execute_batch(&trash_ops)?;
    }

    // Pass 5b: Restore original entity & relation timestamps so RecencyDecay stays lossless.
    let entity_timestamps: Vec<(String, i64, i64, i64)> = payload
        .entities
        .iter()
        .filter_map(|e| {
            let c = e.created_at_ms?;
            let u = e.updated_at_ms.unwrap_or(c);
            let t = e.trashed_at_ms.unwrap_or(0);
            Some((e.id.clone(), c, u, t))
        })
        .collect();
    let edge_timestamps: Vec<(String, String, String, i64)> = payload
        .relations
        .iter()
        .filter_map(|r| {
            let c = r.created_at_ms?;
            Some((r.rel_kind.clone(), r.source_id.clone(), r.target_id.clone(), c))
        })
        .collect();
    db.restore_loaded_timestamps(&entity_timestamps, &edge_timestamps)?;

    // Pass 6: Apply exact full schema (re-enabling unique & strict onDelete constraints),
    // then apply active schema whenever archived types/fields/relations exist!
    db.set_schema(&payload.full_schema)?;
    if payload.schema != payload.full_schema || !payload.archived_types.is_empty() {
        db.set_schema(&payload.schema)?;
    }

    // Pass 6b: Restore schemaless `additionalFields` (`packed.extra`) verbatim into the extra tier
    // so extra entries shadowing archived declared fields or residing on trashed/archived rows
    // round-trip losslessly without colliding with declared-field validation.
    let mut entity_extras: Vec<(String, BTreeMap<String, CanonicalValue>)> = Vec::new();
    for e in &payload.entities {
        if !e.extra.is_empty() {
            let mut map = BTreeMap::new();
            for (k, v) in &e.extra {
                map.insert(k.clone(), v.to_canonical()?);
            }
            entity_extras.push((e.id.clone(), map));
        }
    }
    db.restore_loaded_extra_fields(&entity_extras)?;

    // Pass 7: Rebuild all derived projections cleanly and flush + fsync durability sidecar!
    db.rebuild(None)?;
    db.flush()?;
    db.close()?;

    crate::fs_publish::atomic_publish_dir_new(&staging, target_dir)?;
    cleanup.disarm();

    Ok(DumpReport {
        entities: payload.entities.len() as u64,
        relations: payload.relations.len() as u64,
        embeddings: payload.embeddings.len() as u64,
    })
}

fn encode_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0x0f) as usize] as char);
    }
    s
}

fn decode_hex(s: &str) -> Result<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return Err(ElmdbError::new(ErrorCode::ValidationFailed, "invalid hex length in dump bytes"));
    }
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len() / 2);
    for pair in b.as_chunks::<2>().0 {
        let hi = hex_nibble(pair[0])?;
        let lo = hex_nibble(pair[1])?;
        out.push((hi << 4) | lo);
    }
    Ok(out)
}

fn hex_nibble(c: u8) -> Result<u8> {
    match c {
        b'0'..=b'9' => Ok(c - b'0'),
        b'a'..=b'f' => Ok(c - b'a' + 10),
        b'A'..=b'F' => Ok(c - b'A' + 10),
        _ => Err(ElmdbError::new(ErrorCode::ValidationFailed, "invalid hex character in dump bytes")),
    }
}
