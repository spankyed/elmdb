//! Portable primary-state dump and load (`elmdb dump` / `elmdb load`).
//!
//! Exports schema + all entities (including trashed) + all relations + all primary embeddings into
//! a single JSON file, and reloads into a brand-new database rebuilding all derived indexes cleanly!

use std::fs;
use std::path::Path;

use elmdb_engine::RawSchema;
use elmdb_wire::{ElmdbError, EntityTarget, ErrorCode, Result, WireOp};
use serde::{Deserialize, Serialize};

use crate::db::{Database, OpenOptions};
use crate::protocol::{DumpReport, RelationRow, Row, compiled_to_raw_schema, json_to_canonical};

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DumpPayload {
    format_version: u32,
    schema: RawSchema,
    entities: Vec<Row>,
    relations: Vec<RelationRow>,
    embeddings: Vec<DumpEmbedding>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DumpEmbedding {
    index: String,
    id: String,
    vectors: Vec<Vec<f32>>,
}

/// Dumps `db`'s schema and primary state (entities, relations, embeddings) to a new file `target`.
pub fn dump(db: &Database, target: impl AsRef<Path>) -> Result<DumpReport> {
    let target = target.as_ref();
    if target.exists() {
        return Err(ElmdbError::new(
            ErrorCode::AlreadyExists,
            format!("dump target '{}' already exists", target.display()),
        ));
    }
    let snap = db.snapshot_for_dump()?;
    let schema = compiled_to_raw_schema(&snap.schema);
    let key_to_ext: std::collections::HashMap<elmdb_engine::EntityKey, String> =
        snap.rows.iter().map(|r| (r.key, r.id.clone())).collect();
    let entities: Vec<Row> = snap.rows.iter().map(Row::from_stored).collect();
    let relations: Vec<RelationRow> = snap
        .edges
        .iter()
        .map(|e| {
            let src_id = key_to_ext.get(&e.src).cloned().unwrap_or_default();
            let dst_id = key_to_ext.get(&e.dst).cloned().unwrap_or_default();
            RelationRow::from_stored(e, src_id, dst_id)
        })
        .collect();
    let embeddings: Vec<DumpEmbedding> =
        snap.embeddings.into_iter().map(|(index, id, vectors)| DumpEmbedding { index, id, vectors }).collect();

    let report = DumpReport {
        entities: entities.len() as u64,
        relations: relations.len() as u64,
        embeddings: embeddings.len() as u64,
    };

    let payload = DumpPayload { format_version: 2, schema, entities, relations, embeddings };
    let bytes = serde_json::to_vec_pretty(&payload)
        .map_err(|e| ElmdbError::new(ErrorCode::Internal, format!("serialize dump: {e}")))?;
    if let Some(parent) = target.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent).map_err(|e| ElmdbError::new(ErrorCode::Io, e.to_string()))?;
    }
    fs::write(target, bytes).map_err(|e| ElmdbError::new(ErrorCode::Io, e.to_string()))?;
    Ok(report)
}

/// Loads a v2 dump file into a brand-new database at `target_dir`, rebuilding all derived indexes.
pub fn load(dump_file: impl AsRef<Path>, target_dir: impl AsRef<Path>) -> Result<DumpReport> {
    let target_dir = target_dir.as_ref();
    if target_dir.join("data.mdb").exists() {
        return Err(ElmdbError::new(
            ErrorCode::AlreadyExists,
            format!("target database '{}' already exists", target_dir.display()),
        ));
    }
    let raw = fs::read(dump_file.as_ref()).map_err(|e| ElmdbError::new(ErrorCode::Io, e.to_string()))?;
    let payload: DumpPayload = serde_json::from_slice(&raw)
        .map_err(|e| ElmdbError::new(ErrorCode::InvalidWireFrame, format!("invalid dump file: {e}")))?;
    if payload.format_version != 2 {
        return Err(ElmdbError::new(
            ErrorCode::InvalidWireFrame,
            format!("unsupported dump format version {}", payload.format_version),
        ));
    }

    let db = Database::open(target_dir, OpenOptions::default())?;
    db.set_raw_schema(&payload.schema)?;

    // Create entities first, then trash any that were trashed, then create relations, then set embeddings.
    let mut create_ops = Vec::with_capacity(payload.entities.len());
    let mut trashed_ids = Vec::new();
    for row in &payload.entities {
        let mut fields = Vec::with_capacity(row.fields.len());
        for (k, v) in &row.fields {
            fields.push((k.clone(), json_to_canonical(v)?));
        }
        create_ops.push(WireOp::Create { type_name: row.type_name.clone(), external_id: Some(row.id.clone()), fields });
        if row.trashed {
            trashed_ids.push(row.id.clone());
        }
    }
    if !create_ops.is_empty() {
        db.execute_batch(&create_ops)?;
    }

    let mut link_ops = Vec::with_capacity(payload.relations.len());
    for rel in &payload.relations {
        let info = rel.info.as_ref().map(json_to_canonical).transpose()?;
        link_ops.push(WireOp::Link {
            source: EntityTarget::ExternalId(rel.source_id.clone()),
            rel_kind: rel.rel_kind.clone(),
            target: EntityTarget::ExternalId(rel.target_id.clone()),
            weight: Some(rel.weight.to_bits()),
            info,
        });
    }
    if !link_ops.is_empty() {
        db.execute_batch(&link_ops)?;
    }

    if !trashed_ids.is_empty() {
        let trash_ops: Vec<WireOp> =
            trashed_ids.into_iter().map(|id| WireOp::Trash { target: EntityTarget::ExternalId(id) }).collect();
        db.execute_batch(&trash_ops)?;
    }

    for emb in &payload.embeddings {
        db.set_embeddings(&emb.index, &emb.id, &emb.vectors)?;
    }
    db.flush()?;
    db.close()?;

    Ok(DumpReport {
        entities: payload.entities.len() as u64,
        relations: payload.relations.len() as u64,
        embeddings: payload.embeddings.len() as u64,
    })
}
