//! Clean-room elmdb v2 external protocol types (single source of truth for TypeScript `.d.ts` and JSON Schema).
//!
//! Every exported wire/JSON boundary type lives here with `#[derive(Serialize, Deserialize)]`.
//! Raw string-named queries (`QuerySpec`) and schemas (`Schema`) are parsed once into integer-ID
//! `LogicalPlan` / `CompiledSchema` at the boundary ("parse, don't validate").

use std::collections::BTreeMap;

use elmdb_engine::{
    Cardinality, CompiledSchema, EntityKey, FieldId, FieldKind, FieldMask, IndexId, IndexKind, OnDeletePolicy,
    RawEndpoint, RawField, RawFieldKind, RawIndex, RawIndexKind, RawRelation, RawSchema, RawType, RelKindId,
    SchemaDiff, StoredEdge, StoredRow,
};
use elmdb_projections::{BuildState, CheckReport as ProjCheckReport, IndexStatus as ProjIndexStatus};
use elmdb_query::{
    Cursor, EdgeDir, ExplainPlan, FilterExpr, FuseMethod, GraphAnalyzeSpec, LogicalNode, LogicalPlan, OrderDir,
    OrderSpec, QueryResult, ScoreSpec, SourceSpec, TerminalSpec, TraverseSpec,
};
use elmdb_storage::{RecoveryReport, RecoveryStatus};
use elmdb_wire::{CanonicalValue, ElmdbError, ErrorCode, Result};
#[cfg(feature = "codegen")]
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
#[cfg(feature = "codegen")]
use ts_rs::TS;

/// Canonical `$id` URL for elmdb v2's JSON Schema.
pub const SCHEMA_LANGUAGE: &str = "https://elmdb.dev/schema/2.json";

// ---------------------------------------------------------------------------
// Schema Protocol Types (TS + JSON Schema)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "codegen", derive(TS, JsonSchema))]
#[serde(rename_all = "camelCase")]
pub enum ProtocolOnDelete {
    #[default]
    Restrict,
    SetNull,
    AllowDangling,
}

impl From<ProtocolOnDelete> for OnDeletePolicy {
    fn from(p: ProtocolOnDelete) -> Self {
        match p {
            ProtocolOnDelete::Restrict => Self::Restrict,
            ProtocolOnDelete::SetNull => Self::SetNull,
            ProtocolOnDelete::AllowDangling => Self::AllowDangling,
        }
    }
}

impl From<OnDeletePolicy> for ProtocolOnDelete {
    fn from(p: OnDeletePolicy) -> Self {
        match p {
            OnDeletePolicy::Restrict => Self::Restrict,
            OnDeletePolicy::SetNull => Self::SetNull,
            OnDeletePolicy::AllowDangling => Self::AllowDangling,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "codegen", derive(TS, JsonSchema))]
#[serde(tag = "type", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum FieldSpec {
    String,
    Number,
    Integer,
    Int64,
    Boolean,
    Bytes,
    Timestamp,
    Enum {
        values: Vec<String>,
    },
    Ref {
        to: Vec<String>,
        #[serde(default)]
        #[cfg_attr(feature = "codegen", ts(as = "Option<ProtocolOnDelete>", optional))]
        on_delete: ProtocolOnDelete,
    },
    List {
        of: Box<FieldSpec>,
    },
    Object,
    Any,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "codegen", derive(TS, JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct FieldDefinition {
    #[serde(flatten)]
    pub kind: FieldSpec,
    #[serde(default)]
    #[cfg_attr(feature = "codegen", ts(as = "Option<bool>", optional))]
    pub optional: bool,
    #[serde(default)]
    #[cfg_attr(feature = "codegen", ts(as = "Option<bool>", optional))]
    pub nullable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "codegen", derive(TS, JsonSchema))]
#[serde(tag = "kind", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum IndexDefinition {
    Value {
        fields: Vec<String>,
        #[serde(default)]
        #[cfg_attr(feature = "codegen", ts(as = "Option<bool>", optional))]
        unique: bool,
    },
    Order {
        field: String,
        locale: String,
    },
    Text {
        fields: Vec<(String, u16)>,
    },
    Trigram {
        fields: Vec<String>,
    },
    Vector {
        fields: Vec<String>,
        dims: u32,
        #[serde(default = "default_vec_quant")]
        #[cfg_attr(feature = "codegen", ts(as = "Option<String>", optional))]
        quantization: String,
    },
    Rank {
        relations: Vec<String>,
        #[serde(default)]
        #[cfg_attr(feature = "codegen", ts(as = "Option<bool>", optional))]
        weighted: bool,
    },
}

fn default_vec_quant() -> String {
    "sq8".to_owned()
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "codegen", derive(TS, JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct TypeDefinition {
    #[serde(default)]
    #[cfg_attr(feature = "codegen", ts(as = "Option<BTreeMap<String, FieldDefinition>>", optional))]
    pub fields: BTreeMap<String, FieldDefinition>,
    #[serde(default)]
    #[cfg_attr(feature = "codegen", ts(as = "Option<BTreeMap<String, IndexDefinition>>", optional))]
    pub indexes: BTreeMap<String, IndexDefinition>,
    #[serde(default)]
    #[cfg_attr(feature = "codegen", ts(as = "Option<bool>", optional))]
    pub additional_fields: bool,
    #[serde(default)]
    #[cfg_attr(feature = "codegen", ts(optional))]
    pub short_code: Option<String>,
    #[serde(default)]
    #[cfg_attr(feature = "codegen", ts(as = "Option<bool>", optional))]
    pub change_order: bool,
    #[serde(default)]
    #[cfg_attr(feature = "codegen", ts(optional))]
    pub declared_by: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "codegen", derive(TS, JsonSchema))]
#[serde(rename_all = "camelCase")]
pub enum ProtocolCardinality {
    #[default]
    ManyToMany,
    ManyToOne,
    OneToMany,
    OneToOne,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "codegen", derive(TS, JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct RelationEndpoint {
    pub source: String,
    pub target: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "codegen", derive(TS, JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct RelationDefinition {
    #[serde(default)]
    #[cfg_attr(feature = "codegen", ts(as = "Option<ProtocolCardinality>", optional))]
    pub cardinality: ProtocolCardinality,
    #[serde(default)]
    #[cfg_attr(feature = "codegen", ts(as = "Option<bool>", optional))]
    pub acyclic: bool,
    #[serde(default)]
    #[cfg_attr(feature = "codegen", ts(as = "Option<bool>", optional))]
    pub weighted: bool,
    #[serde(default)]
    #[cfg_attr(feature = "codegen", ts(as = "Option<Vec<RelationEndpoint>>", optional))]
    pub endpoints: Vec<RelationEndpoint>,
    #[serde(default)]
    #[cfg_attr(feature = "codegen", ts(optional))]
    pub declared_by: Option<String>,
}

/// Top-level elmdb v2 schema definition (exported to TS & JSON Schema).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "codegen", derive(TS, JsonSchema))]
#[cfg_attr(feature = "codegen", ts(export))]
#[serde(rename_all = "camelCase")]
pub struct Schema {
    #[serde(rename = "$schema", default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "codegen", ts(optional))]
    pub dollar_schema: Option<String>,
    #[serde(default)]
    #[cfg_attr(feature = "codegen", ts(as = "Option<BTreeMap<String, TypeDefinition>>", optional))]
    pub types: BTreeMap<String, TypeDefinition>,
    #[serde(default)]
    #[cfg_attr(feature = "codegen", ts(as = "Option<BTreeMap<String, RelationDefinition>>", optional))]
    pub relations: BTreeMap<String, RelationDefinition>,
}

impl From<&FieldSpec> for RawFieldKind {
    fn from(s: &FieldSpec) -> Self {
        match s {
            FieldSpec::String => Self::String,
            FieldSpec::Number => Self::Number,
            FieldSpec::Integer => Self::Integer,
            FieldSpec::Int64 => Self::Int64,
            FieldSpec::Boolean => Self::Boolean,
            FieldSpec::Bytes => Self::Bytes,
            FieldSpec::Timestamp => Self::Timestamp,
            FieldSpec::Enum { values } => Self::Enum { values: values.clone() },
            FieldSpec::Ref { to, on_delete } => Self::Ref { to: to.clone(), on_delete: (*on_delete).into() },
            FieldSpec::List { of } => Self::List { of: Box::new(RawFieldKind::from(of.as_ref())) },
            FieldSpec::Object => Self::Object,
            FieldSpec::Any => Self::Any,
        }
    }
}

impl From<&Schema> for RawSchema {
    fn from(s: &Schema) -> Self {
        let mut types = BTreeMap::new();
        for (tname, tdef) in &s.types {
            let fields = tdef
                .fields
                .iter()
                .map(|(fnm, fdef)| {
                    (
                        fnm.clone(),
                        RawField { kind: (&fdef.kind).into(), optional: fdef.optional, nullable: fdef.nullable },
                    )
                })
                .collect();
            let indexes = tdef
                .indexes
                .iter()
                .map(|(inm, idef)| {
                    let kind = match idef {
                        IndexDefinition::Value { fields, unique } => {
                            RawIndexKind::Value { fields: fields.clone(), unique: *unique }
                        }
                        IndexDefinition::Order { field, locale } => {
                            RawIndexKind::Order { field: field.clone(), locale: locale.clone() }
                        }
                        IndexDefinition::Text { fields } => RawIndexKind::Text { fields: fields.clone() },
                        IndexDefinition::Trigram { fields } => RawIndexKind::Trigram { fields: fields.clone() },
                        IndexDefinition::Vector { fields, dims, quantization } => RawIndexKind::Vector {
                            fields: fields.clone(),
                            dims: *dims,
                            quantization: quantization.clone(),
                        },
                        IndexDefinition::Rank { relations, weighted } => {
                            RawIndexKind::Rank { relations: relations.clone(), weighted: *weighted }
                        }
                    };
                    (inm.clone(), RawIndex { kind })
                })
                .collect();
            types.insert(
                tname.clone(),
                RawType {
                    fields,
                    indexes,
                    additional_fields: tdef.additional_fields,
                    short_code: tdef.short_code.clone(),
                    change_order: tdef.change_order,
                    declared_by: tdef.declared_by.clone(),
                },
            );
        }
        let mut relations = BTreeMap::new();
        for (rname, rdef) in &s.relations {
            relations.insert(
                rname.clone(),
                RawRelation {
                    cardinality: match rdef.cardinality {
                        ProtocolCardinality::ManyToMany => Cardinality::ManyToMany,
                        ProtocolCardinality::ManyToOne => Cardinality::ManyToOne,
                        ProtocolCardinality::OneToMany => Cardinality::OneToMany,
                        ProtocolCardinality::OneToOne => Cardinality::OneToOne,
                    },
                    acyclic: rdef.acyclic,
                    weighted: rdef.weighted,
                    endpoints: rdef
                        .endpoints
                        .iter()
                        .map(|e| RawEndpoint { source: e.source.clone(), target: e.target.clone() })
                        .collect(),
                    declared_by: rdef.declared_by.clone(),
                },
            );
        }
        RawSchema { types, relations }
    }
}

// ---------------------------------------------------------------------------
// Reports, Admin & Stats Protocol Types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "codegen", derive(TS))]
#[cfg_attr(feature = "codegen", ts(export))]
#[serde(rename_all = "camelCase")]
pub struct OpenReport {
    pub status: String,
    pub durable_txnid: Option<u64>,
    pub observed_txnid: u64,
    pub valid_slots: u8,
}

impl From<&RecoveryReport> for OpenReport {
    fn from(r: &RecoveryReport) -> Self {
        let status = match r.status {
            RecoveryStatus::Fresh => "fresh",
            RecoveryStatus::Clean => "clean",
            RecoveryStatus::SameBootUnsyncedTail => "sameBootUnsyncedTail",
            RecoveryStatus::RebootVerified => "rebootVerified",
            RecoveryStatus::UnsyncedAfterReboot => "unsyncedAfterReboot",
        };
        Self {
            status: status.to_owned(),
            durable_txnid: r.durable.map(|d| d.txnid),
            observed_txnid: r.observed_txnid,
            valid_slots: r.valid_slots,
        }
    }
}

/// Converts a compiled `CompiledSchema` back into a portable `RawSchema` (for `dump` and `getSchema`).
pub fn compiled_to_raw_schema(schema: &CompiledSchema) -> RawSchema {
    let mut types = BTreeMap::new();
    for td in &schema.types {
        if td.archived {
            continue;
        }
        let mut fields = BTreeMap::new();
        for fd in &td.fields {
            if fd.archived {
                continue;
            }
            fields.insert(
                fd.name.clone(),
                RawField { kind: field_kind_to_raw(schema, &fd.kind), optional: fd.optional, nullable: fd.nullable },
            );
        }
        let mut indexes = BTreeMap::new();
        for idx in &schema.indexes {
            if idx.type_id != td.id {
                continue;
            }
            let fname = |fid: FieldId| td.field_by_id(fid).map(|f| f.name.clone()).unwrap_or_default();
            let kind = match &idx.kind {
                IndexKind::Value { fields: fs, unique } => {
                    RawIndexKind::Value { fields: fs.iter().copied().map(fname).collect(), unique: *unique }
                }
                IndexKind::Order { field, locale } => {
                    RawIndexKind::Order { field: fname(*field), locale: locale.clone() }
                }
                IndexKind::Text { fields: fs } => {
                    RawIndexKind::Text { fields: fs.iter().map(|(fid, w)| (fname(*fid), *w)).collect() }
                }
                IndexKind::Trigram { fields: fs } => {
                    RawIndexKind::Trigram { fields: fs.iter().copied().map(fname).collect() }
                }
                IndexKind::Vector { fields: fs, dims, quantization } => RawIndexKind::Vector {
                    fields: fs.iter().copied().map(fname).collect(),
                    dims: *dims,
                    quantization: quantization.clone(),
                },
                IndexKind::Rank { relations, weighted } => RawIndexKind::Rank {
                    relations: relations
                        .iter()
                        .filter_map(|rk| schema.relation_by_id(*rk).map(|r| r.name.clone()))
                        .collect(),
                    weighted: *weighted,
                },
            };
            indexes.insert(idx.name.clone(), RawIndex { kind });
        }
        types.insert(
            td.name.clone(),
            RawType {
                fields,
                indexes,
                additional_fields: td.additional_fields,
                short_code: td.short_code.clone(),
                change_order: td.change_order,
                declared_by: td.declared_by.clone(),
            },
        );
    }

    let mut relations = BTreeMap::new();
    for rd in &schema.relations {
        if rd.archived {
            continue;
        }
        let endpoints = rd
            .endpoints
            .iter()
            .filter_map(|(s, t)| {
                Some(RawEndpoint {
                    source: schema.type_by_id(*s)?.name.clone(),
                    target: schema.type_by_id(*t)?.name.clone(),
                })
            })
            .collect();
        relations.insert(
            rd.name.clone(),
            RawRelation {
                cardinality: rd.cardinality,
                acyclic: rd.acyclic,
                weighted: rd.weighted,
                endpoints,
                declared_by: rd.declared_by.clone(),
            },
        );
    }
    RawSchema { types, relations }
}

/// Converts a `CompiledSchema` into the canonical external `protocol::Schema` (camelCase JSON & TS contract).
/// When `include_archived` is `true`, archived types/fields/relations are included as well (used by lossless `dump`).
pub fn compiled_to_protocol_schema(schema: &CompiledSchema, include_archived: bool) -> Schema {
    let mut types = BTreeMap::new();
    for td in &schema.types {
        if td.archived && !include_archived {
            continue;
        }
        let mut fields = BTreeMap::new();
        for fd in &td.fields {
            if fd.archived && !include_archived {
                continue;
            }
            fields.insert(
                fd.name.clone(),
                FieldDefinition {
                    kind: field_kind_to_spec(schema, &fd.kind),
                    optional: fd.optional,
                    nullable: fd.nullable,
                },
            );
        }
        let mut indexes = BTreeMap::new();
        for idx in schema.indexes.iter().filter(|i| i.type_id == td.id) {
            let kind = match &idx.kind {
                IndexKind::Value { fields: fids, unique } => IndexDefinition::Value {
                    fields: fids.iter().filter_map(|fid| td.field_by_id(*fid).map(|f| f.name.clone())).collect(),
                    unique: *unique,
                },
                IndexKind::Order { field, locale } => IndexDefinition::Order {
                    field: td.field_by_id(*field).map(|f| f.name.clone()).unwrap_or_default(),
                    locale: locale.clone(),
                },
                IndexKind::Text { fields: fids } => IndexDefinition::Text {
                    fields: fids
                        .iter()
                        .filter_map(|(fid, w)| td.field_by_id(*fid).map(|f| (f.name.clone(), *w)))
                        .collect(),
                },
                IndexKind::Trigram { fields: fids } => IndexDefinition::Trigram {
                    fields: fids.iter().filter_map(|fid| td.field_by_id(*fid).map(|f| f.name.clone())).collect(),
                },
                IndexKind::Vector { fields: fids, dims, quantization } => IndexDefinition::Vector {
                    fields: fids.iter().filter_map(|fid| td.field_by_id(*fid).map(|f| f.name.clone())).collect(),
                    dims: *dims,
                    quantization: quantization.clone(),
                },
                IndexKind::Rank { relations: rids, weighted } => IndexDefinition::Rank {
                    relations: rids
                        .iter()
                        .filter_map(|rid| schema.relation_by_id(*rid).map(|r| r.name.clone()))
                        .collect(),
                    weighted: *weighted,
                },
            };
            indexes.insert(idx.name.clone(), kind);
        }
        types.insert(
            td.name.clone(),
            TypeDefinition {
                fields,
                indexes,
                additional_fields: td.additional_fields,
                short_code: td.short_code.clone(),
                change_order: td.change_order,
                declared_by: td.declared_by.clone(),
            },
        );
    }

    let mut relations = BTreeMap::new();
    for rd in &schema.relations {
        if rd.archived && !include_archived {
            continue;
        }
        let endpoints = rd
            .endpoints
            .iter()
            .filter_map(|(s, t)| {
                Some(RelationEndpoint {
                    source: schema.type_by_id(*s)?.name.clone(),
                    target: schema.type_by_id(*t)?.name.clone(),
                })
            })
            .collect();
        let cardinality = match rd.cardinality {
            Cardinality::ManyToMany => ProtocolCardinality::ManyToMany,
            Cardinality::ManyToOne => ProtocolCardinality::ManyToOne,
            Cardinality::OneToMany => ProtocolCardinality::OneToMany,
            Cardinality::OneToOne => ProtocolCardinality::OneToOne,
        };
        relations.insert(
            rd.name.clone(),
            RelationDefinition {
                cardinality,
                acyclic: rd.acyclic,
                weighted: rd.weighted,
                endpoints,
                declared_by: rd.declared_by.clone(),
            },
        );
    }

    Schema { dollar_schema: Some(SCHEMA_LANGUAGE.to_owned()), types, relations }
}

fn field_kind_to_spec(schema: &CompiledSchema, kind: &FieldKind) -> FieldSpec {
    match kind {
        FieldKind::String => FieldSpec::String,
        FieldKind::Number => FieldSpec::Number,
        FieldKind::Integer => FieldSpec::Integer,
        FieldKind::Int64 => FieldSpec::Int64,
        FieldKind::Boolean => FieldSpec::Boolean,
        FieldKind::Bytes => FieldSpec::Bytes,
        FieldKind::Timestamp => FieldSpec::Timestamp,
        FieldKind::Enum(values) => FieldSpec::Enum { values: values.clone() },
        FieldKind::Ref { to, on_delete } => FieldSpec::Ref {
            to: to.iter().filter_map(|tid| schema.type_by_id(*tid).map(|t| t.name.clone())).collect(),
            on_delete: (*on_delete).into(),
        },
        FieldKind::List(of) => FieldSpec::List { of: Box::new(field_kind_to_spec(schema, of)) },
        FieldKind::Object => FieldSpec::Object,
        FieldKind::Any => FieldSpec::Any,
    }
}

fn field_kind_to_raw(schema: &CompiledSchema, kind: &FieldKind) -> RawFieldKind {
    match kind {
        FieldKind::String => RawFieldKind::String,
        FieldKind::Number => RawFieldKind::Number,
        FieldKind::Integer => RawFieldKind::Integer,
        FieldKind::Int64 => RawFieldKind::Int64,
        FieldKind::Boolean => RawFieldKind::Boolean,
        FieldKind::Bytes => RawFieldKind::Bytes,
        FieldKind::Timestamp => RawFieldKind::Timestamp,
        FieldKind::Enum(values) => RawFieldKind::Enum { values: values.clone() },
        FieldKind::Ref { to, on_delete } => RawFieldKind::Ref {
            to: to.iter().filter_map(|tid| schema.type_by_id(*tid).map(|t| t.name.clone())).collect(),
            on_delete: *on_delete,
        },
        FieldKind::List(of) => RawFieldKind::List { of: Box::new(field_kind_to_raw(schema, of)) },
        FieldKind::Object => RawFieldKind::Object,
        FieldKind::Any => RawFieldKind::Any,
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "codegen", derive(TS))]
#[cfg_attr(feature = "codegen", ts(export))]
#[serde(rename_all = "camelCase")]
pub struct SchemaChange {
    pub epoch: u64,
    pub added_indexes: Vec<String>,
    pub removed_index_ids: Vec<u32>,
}

impl SchemaChange {
    pub fn from_diff(schema: &CompiledSchema, diff: &SchemaDiff) -> Self {
        Self {
            epoch: schema.epoch,
            added_indexes: diff
                .added_indexes
                .iter()
                .filter_map(|id| schema.index_by_id(*id).map(|i| i.name.clone()))
                .collect(),
            removed_index_ids: diff.removed_indexes.iter().map(|i| i.0).collect(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "codegen", derive(TS))]
#[cfg_attr(feature = "codegen", ts(export))]
#[serde(rename_all = "camelCase")]
pub struct BatchResult {
    pub commit_seq: u64,
    pub txnid: u64,
    pub created_ids: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "codegen", derive(TS))]
#[cfg_attr(feature = "codegen", ts(export))]
#[serde(rename_all = "camelCase")]
pub struct IndexStatus {
    pub name: String,
    pub type_name: String,
    pub state: String,
    pub built_up_to: Option<u64>,
    pub high_water: Option<u64>,
    pub error: Option<String>,
}

impl IndexStatus {
    pub fn from_proj(schema: &CompiledSchema, st: &ProjIndexStatus) -> Self {
        let type_name = schema
            .index_by_id(st.index_id)
            .and_then(|d| schema.type_by_id(d.type_id))
            .map(|t| t.name.clone())
            .unwrap_or_default();
        let (state, built_up_to, high_water, error) = match &st.state {
            BuildState::Ready => ("ready".to_owned(), None, None, None),
            BuildState::Building { built_up_to, high_water } => {
                ("building".to_owned(), Some(built_up_to.0), Some(high_water.0), None)
            }
            BuildState::Failed { message, .. } => ("failed".to_owned(), None, None, Some(message.clone())),
        };
        Self { name: st.name.clone(), type_name, state, built_up_to, high_water, error }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "codegen", derive(TS))]
#[cfg_attr(feature = "codegen", ts(export))]
#[serde(rename_all = "camelCase")]
pub struct CheckReport {
    pub clean: bool,
    pub checked_indexes: usize,
    pub mismatches: Vec<String>,
}

impl CheckReport {
    pub fn is_clean(&self) -> bool {
        self.clean
    }

    pub fn from_proj(r: ProjCheckReport) -> Self {
        let clean = r.is_ok();
        let mismatches = r.mismatches.into_iter().map(|m| format!("{}: {}", m.index_name, m.detail)).collect();
        Self { clean, checked_indexes: r.checked_indexes, mismatches }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "codegen", derive(TS))]
#[cfg_attr(feature = "codegen", ts(export))]
#[serde(rename_all = "camelCase")]
pub struct DatabaseStats {
    pub schema_epoch: u64,
    pub commit_seq: u64,
    pub txnid: u64,
    pub type_counts: BTreeMap<String, u64>,
    pub index_count: usize,
    pub active_watches: usize,
    pub tier2_live_ranges: usize,
    pub tier2_slab_bytes: usize,
    pub plan_cache_hits: u64,
    pub plan_cache_misses: u64,
    pub map_size_bytes: u64,
    pub page_size_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "codegen", derive(TS))]
#[cfg_attr(feature = "codegen", ts(export))]
#[serde(rename_all = "camelCase")]
pub struct DumpReport {
    pub entities: u64,
    pub relations: u64,
    pub embeddings: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "codegen", derive(TS))]
#[cfg_attr(feature = "codegen", ts(export))]
#[serde(rename_all = "camelCase")]
pub struct WatchTick {
    pub due_ids: Vec<u64>,
    pub next_wake_delay_ms: Option<u64>,
    pub settled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "codegen", derive(TS))]
#[cfg_attr(feature = "codegen", ts(export))]
#[serde(rename_all = "camelCase")]
pub struct Row {
    pub id: String,
    pub type_name: String,
    pub trashed: bool,
    #[cfg_attr(feature = "codegen", ts(type = "Record<string, unknown>"))]
    pub fields: BTreeMap<String, serde_json::Value>,
}

impl Row {
    pub fn from_stored(r: &StoredRow) -> Self {
        let fields = r.fields.iter().map(|(k, v)| (k.clone(), canonical_to_json(v))).collect();
        Self { id: r.id.clone(), type_name: r.type_name.clone(), trashed: r.trashed, fields }
    }

    /// Materializes a `StoredRow` into an external `Row`, resolving all `EntityRef`s via `resolve_key`.
    pub fn from_stored_resolved<F>(r: &StoredRow, resolve_key: &mut F) -> Result<Self>
    where
        F: FnMut(EntityKey) -> Result<String>,
    {
        let mut fields = BTreeMap::new();
        for (k, v) in &r.fields {
            fields.insert(k.clone(), canonical_to_json_resolved(v, resolve_key)?);
        }
        Ok(Self { id: r.id.clone(), type_name: r.type_name.clone(), trashed: r.trashed, fields })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "codegen", derive(TS))]
#[cfg_attr(feature = "codegen", ts(export))]
#[serde(rename_all = "camelCase")]
pub struct RelationRow {
    pub rel_kind: String,
    pub source_id: String,
    pub target_id: String,
    pub weight: f64,
    #[cfg_attr(feature = "codegen", ts(type = "unknown | null"))]
    pub info: Option<serde_json::Value>,
}

impl RelationRow {
    pub fn from_stored(e: &StoredEdge, source_id: String, target_id: String) -> Self {
        Self {
            rel_kind: e.kind_name.clone(),
            source_id,
            target_id,
            weight: e.weight,
            info: e.info.as_ref().map(canonical_to_json),
        }
    }
}

// ---------------------------------------------------------------------------
// Boundary Query Spec (string-named JSON -> parsed integer-ID `LogicalPlan`)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "codegen", derive(TS))]
#[serde(tag = "kind", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum QuerySource {
    Type {
        type_name: String,
        #[serde(default)]
        #[cfg_attr(feature = "codegen", ts(as = "Option<bool>", optional))]
        include_trashed: bool,
    },
    Types {
        type_names: Vec<String>,
        #[serde(default)]
        #[cfg_attr(feature = "codegen", ts(as = "Option<bool>", optional))]
        include_trashed: bool,
    },
    AllEntities {
        #[serde(default)]
        #[cfg_attr(feature = "codegen", ts(as = "Option<bool>", optional))]
        include_trashed: bool,
    },
    Ids {
        ids: Vec<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "codegen", derive(TS))]
#[serde(tag = "op", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum QueryFilter {
    Eq {
        field: String,
        #[cfg_attr(feature = "codegen", ts(type = "unknown"))]
        value: serde_json::Value,
    },
    Ne {
        field: String,
        #[cfg_attr(feature = "codegen", ts(type = "unknown"))]
        value: serde_json::Value,
    },
    In {
        field: String,
        #[cfg_attr(feature = "codegen", ts(type = "Array<unknown>"))]
        values: Vec<serde_json::Value>,
    },
    Range {
        field: String,
        #[serde(default)]
        #[cfg_attr(feature = "codegen", ts(optional, type = "[unknown, boolean] | null"))]
        low: Option<(serde_json::Value, bool)>,
        #[serde(default)]
        #[cfg_attr(feature = "codegen", ts(optional, type = "[unknown, boolean] | null"))]
        high: Option<(serde_json::Value, bool)>,
    },
    StartsWith {
        field: String,
        prefix: String,
    },
    Contains {
        field: String,
        needle: String,
    },
    Regex {
        field: String,
        pattern: String,
    },
    IsNull {
        field: String,
    },
    IsPresent {
        field: String,
    },
    HasLink {
        #[serde(default)]
        #[cfg_attr(feature = "codegen", ts(as = "Option<Vec<String>>", optional))]
        rel_kinds: Vec<String>,
        dir: String,
        #[serde(default)]
        #[cfg_attr(feature = "codegen", ts(optional))]
        target_id: Option<String>,
    },
    And {
        filters: Vec<QueryFilter>,
    },
    Or {
        filters: Vec<QueryFilter>,
    },
    Not {
        filter: Box<QueryFilter>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "codegen", derive(TS))]
#[serde(tag = "kind", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum QueryScore {
    Bm25f {
        index: String,
        query: String,
        #[serde(default)]
        #[cfg_attr(feature = "codegen", ts(as = "Option<bool>", optional))]
        require_all: bool,
        #[serde(default)]
        #[cfg_attr(feature = "codegen", ts(as = "Option<bool>", optional))]
        prefix: bool,
        #[serde(default)]
        #[cfg_attr(feature = "codegen", ts(optional))]
        phrase: Option<Vec<String>>,
        #[serde(default)]
        #[cfg_attr(feature = "codegen", ts(optional))]
        top_k: Option<usize>,
    },
    VectorNear {
        index: String,
        vector: Vec<f32>,
        top_k: usize,
    },
    VectorLike {
        index: String,
        seed_id: String,
        top_k: usize,
    },
    RankIndex {
        index: String,
    },
    FuzzyLabel {
        field: String,
        query: String,
        max_distance: u8,
    },
    RecencyDecay {
        field: String,
        now_ms: i64,
        half_life_ms: f64,
    },
    Fuse {
        inputs: Vec<(QueryScore, f64)>,
        #[serde(default)]
        #[cfg_attr(feature = "codegen", ts(as = "Option<String>", optional))]
        method: String,
        #[serde(default)]
        #[cfg_attr(feature = "codegen", ts(optional))]
        rrf_k: Option<f64>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "codegen", derive(TS))]
#[serde(tag = "kind", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum QueryStep {
    Filter {
        expr: QueryFilter,
    },
    Score {
        spec: QueryScore,
    },
    Hops {
        #[serde(default)]
        #[cfg_attr(feature = "codegen", ts(as = "Option<Vec<String>>", optional))]
        rel_kinds: Vec<String>,
        dir: String,
        min_hops: u32,
        max_hops: u32,
    },
    ShortestPath {
        #[serde(default)]
        #[cfg_attr(feature = "codegen", ts(as = "Option<Vec<String>>", optional))]
        rel_kinds: Vec<String>,
        dir: String,
        target_id: String,
        max_hops: u32,
    },
    Distinct,
    OrderSlice {
        by: String,
        #[serde(default)]
        #[cfg_attr(feature = "codegen", ts(optional))]
        field: Option<String>,
        dir: String,
        #[serde(default)]
        #[cfg_attr(feature = "codegen", ts(optional))]
        locale: Option<String>,
        #[serde(default)]
        #[cfg_attr(feature = "codegen", ts(optional))]
        after_id: Option<String>,
        #[serde(default)]
        #[cfg_attr(feature = "codegen", ts(optional))]
        after_score: Option<f64>,
        #[serde(default)]
        #[cfg_attr(feature = "codegen", ts(optional, type = "unknown | null"))]
        after_value: Option<serde_json::Value>,
        #[serde(default)]
        #[cfg_attr(feature = "codegen", ts(optional))]
        limit: Option<u64>,
    },
    GraphPpr {
        #[serde(default)]
        #[cfg_attr(feature = "codegen", ts(as = "Option<Vec<String>>", optional))]
        rel_kinds: Vec<String>,
        #[serde(default)]
        #[cfg_attr(feature = "codegen", ts(as = "Option<bool>", optional))]
        weighted: bool,
        seeds: Vec<(String, f64)>,
        damping: f64,
        iterations: u32,
        tolerance: f64,
    },
    GraphWeakComponents {
        #[serde(default)]
        #[cfg_attr(feature = "codegen", ts(as = "Option<Vec<String>>", optional))]
        rel_kinds: Vec<String>,
    },
    GraphScc {
        #[serde(default)]
        #[cfg_attr(feature = "codegen", ts(as = "Option<Vec<String>>", optional))]
        rel_kinds: Vec<String>,
    },
    GraphTopoSort {
        #[serde(default)]
        #[cfg_attr(feature = "codegen", ts(as = "Option<Vec<String>>", optional))]
        rel_kinds: Vec<String>,
    },
    GraphLeiden {
        #[serde(default)]
        #[cfg_attr(feature = "codegen", ts(as = "Option<Vec<String>>", optional))]
        rel_kinds: Vec<String>,
        #[serde(default)]
        #[cfg_attr(feature = "codegen", ts(as = "Option<bool>", optional))]
        weighted: bool,
        resolution: f64,
        seed: u64,
        iterations: u32,
    },
    GraphJaccard {
        #[serde(default)]
        #[cfg_attr(feature = "codegen", ts(as = "Option<Vec<String>>", optional))]
        rel_kinds: Vec<String>,
        dir: String,
        seed_id: String,
        top_k: usize,
    },
    GraphAdamicAdar {
        #[serde(default)]
        #[cfg_attr(feature = "codegen", ts(as = "Option<Vec<String>>", optional))]
        rel_kinds: Vec<String>,
        dir: String,
        seed_id: String,
        top_k: usize,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "codegen", derive(TS))]
#[serde(tag = "kind", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum QueryTerminal {
    Ids,
    Rows {
        #[serde(default)]
        #[cfg_attr(feature = "codegen", ts(optional))]
        fields: Option<Vec<String>>,
    },
    Hits {
        #[serde(default)]
        #[cfg_attr(feature = "codegen", ts(optional))]
        fields: Option<Vec<String>>,
        #[serde(default)]
        #[cfg_attr(feature = "codegen", ts(as = "Option<bool>", optional))]
        highlights: bool,
    },
    Count,
    Exists,
    Sum {
        field: String,
    },
    MinMax {
        field: String,
    },
    GroupBy {
        field: String,
        #[serde(default)]
        #[cfg_attr(feature = "codegen", ts(as = "Option<bool>", optional))]
        counts_only: bool,
    },
    Degrees {
        #[serde(default)]
        #[cfg_attr(feature = "codegen", ts(as = "Option<Vec<String>>", optional))]
        rel_kinds: Vec<String>,
        dir: String,
    },
}

/// Complete string-named query specification sent across FFI / JSON boundary.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "codegen", derive(TS))]
#[cfg_attr(feature = "codegen", ts(export))]
#[serde(rename_all = "camelCase")]
pub struct QuerySpec {
    pub source: QuerySource,
    #[serde(default)]
    #[cfg_attr(feature = "codegen", ts(as = "Option<Vec<QueryStep>>", optional))]
    pub steps: Vec<QueryStep>,
    #[serde(default)]
    #[cfg_attr(feature = "codegen", ts(optional))]
    pub terminal: Option<QueryTerminal>,
    #[serde(default)]
    #[cfg_attr(feature = "codegen", ts(as = "Option<bool>", optional))]
    pub explain: bool,
}

/// Single UTF-16 code-unit highlight span `(element_index, utf16_start, utf16_end)`.
pub type HighlightSpan = (u32, usize, usize);
/// Per-field UTF-16 search highlight spans map.
pub type SearchHighlights = BTreeMap<String, Vec<HighlightSpan>>;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "codegen", derive(TS))]
#[serde(rename_all = "camelCase")]
pub struct Hit {
    pub id: String,
    pub score: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "codegen", ts(optional))]
    pub row: Option<Row>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "codegen", ts(optional, type = "Record<string, Array<[number, number, number]>>"))]
    pub highlights: Option<SearchHighlights>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "codegen", derive(TS))]
#[serde(tag = "kind", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum QueryAnswerBody {
    Ids {
        ids: Vec<String>,
    },
    Rows {
        rows: Vec<Row>,
    },
    Hits {
        hits: Vec<Hit>,
    },
    Count {
        count: u64,
    },
    Exists {
        exists: bool,
    },
    Sum {
        sum: f64,
    },
    MinMax {
        #[cfg_attr(feature = "codegen", ts(type = "unknown | null"))]
        min: Option<serde_json::Value>,
        #[cfg_attr(feature = "codegen", ts(type = "unknown | null"))]
        max: Option<serde_json::Value>,
    },
    GroupCounts {
        #[cfg_attr(feature = "codegen", ts(type = "Array<[unknown, number]>"))]
        groups: Vec<(serde_json::Value, u64)>,
    },
    Groups {
        #[cfg_attr(feature = "codegen", ts(type = "Array<[unknown, Array<string>]>"))]
        groups: Vec<(serde_json::Value, Vec<String>)>,
    },
    Degrees {
        degrees: Vec<(String, u64)>,
    },
    GraphGroups {
        groups: Vec<Vec<String>>,
    },
    GraphTopo {
        order: Vec<String>,
        cycles: u32,
    },
    GraphPpr {
        scores: Vec<Hit>,
        converged: bool,
        iterations: u32,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "codegen", derive(TS))]
#[cfg_attr(feature = "codegen", ts(export))]
#[serde(rename_all = "camelCase")]
pub struct QueryAnswer {
    #[serde(flatten)]
    pub body: QueryAnswerBody,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "codegen", ts(type = "unknown | null"))]
    pub explain: Option<serde_json::Value>,
}

// ---------------------------------------------------------------------------
// Value & Plan Boundary Conversion Helpers ("Parse, don't validate")
// ---------------------------------------------------------------------------

pub fn json_to_canonical(v: &serde_json::Value) -> Result<CanonicalValue> {
    json_to_canonical_depth(v, 0)
}

fn json_to_canonical_depth(v: &serde_json::Value, depth: usize) -> Result<CanonicalValue> {
    if depth > elmdb_wire::MAX_VALUE_DEPTH {
        return Err(ElmdbError::new(
            ErrorCode::ValidationFailed,
            format!("JSON value nesting depth exceeds maximum {}", elmdb_wire::MAX_VALUE_DEPTH),
        ));
    }
    match v {
        serde_json::Value::Null => Ok(CanonicalValue::Null),
        serde_json::Value::Bool(b) => Ok(CanonicalValue::Bool(*b)),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Ok(CanonicalValue::Int(i))
            } else if let Some(u) = n.as_u64() {
                if let Ok(i) = i64::try_from(u) {
                    Ok(CanonicalValue::Int(i))
                } else {
                    Ok(CanonicalValue::Float(u as f64))
                }
            } else if let Some(f) = n.as_f64() {
                if !f.is_finite() {
                    return Err(ElmdbError::new(ErrorCode::InvalidArgument, "non-finite float"));
                }
                Ok(CanonicalValue::Float(f))
            } else {
                Err(ElmdbError::new(ErrorCode::InvalidArgument, "unsupported JSON number"))
            }
        }
        serde_json::Value::String(s) => Ok(CanonicalValue::String(s.clone())),
        serde_json::Value::Array(xs) => {
            let mut out = Vec::with_capacity(xs.len());
            for x in xs {
                out.push(json_to_canonical_depth(x, depth + 1)?);
            }
            Ok(CanonicalValue::Array(out))
        }
        serde_json::Value::Object(map) => {
            let mut out = Vec::with_capacity(map.len());
            for (k, val) in map {
                if k == "__proto__" {
                    return Err(ElmdbError::new(ErrorCode::ValidationFailed, "reserved key '__proto__'"));
                }
                out.push((k.clone(), json_to_canonical_depth(val, depth + 1)?));
            }
            CanonicalValue::object(out)
        }
    }
}

pub fn canonical_to_json(v: &CanonicalValue) -> serde_json::Value {
    match v {
        CanonicalValue::Null => serde_json::Value::Null,
        CanonicalValue::Bool(b) => serde_json::Value::Bool(*b),
        CanonicalValue::Int(i) => serde_json::Value::Number((*i).into()),
        CanonicalValue::Float(f) => {
            serde_json::Number::from_f64(*f).map_or(serde_json::Value::Null, serde_json::Value::Number)
        }
        CanonicalValue::Timestamp { ms, .. } => serde_json::Value::Number((*ms).into()),
        CanonicalValue::String(s) | CanonicalValue::DanglingRef(s) => serde_json::Value::String(s.clone()),
        CanonicalValue::EntityRef(k) => serde_json::Value::String(format!("__entity_key:{k}")),
        CanonicalValue::Bytes(b) => serde_json::Value::Array(b.iter().map(|x| serde_json::Value::from(*x)).collect()),
        CanonicalValue::Array(xs) => serde_json::Value::Array(xs.iter().map(canonical_to_json).collect()),
        CanonicalValue::Object(map) => {
            let mut out = serde_json::Map::new();
            for (k, val) in map {
                out.insert(k.clone(), canonical_to_json(val));
            }
            serde_json::Value::Object(out)
        }
    }
}

pub fn parse_edge_dir(s: &str) -> Result<EdgeDir> {
    match s {
        "out" | "Out" => Ok(EdgeDir::Out),
        "in" | "In" => Ok(EdgeDir::In),
        "both" | "Both" => Ok(EdgeDir::Both),
        other => Err(ElmdbError::new(ErrorCode::InvalidArgument, format!("invalid edge dir '{other}'"))),
    }
}

fn parse_order_dir(s: &str) -> Result<OrderDir> {
    match s {
        "asc" | "Asc" => Ok(OrderDir::Asc),
        "desc" | "Desc" => Ok(OrderDir::Desc),
        other => Err(ElmdbError::new(ErrorCode::InvalidArgument, format!("invalid order dir '{other}'"))),
    }
}

fn resolve_rel_kinds(schema: &CompiledSchema, names: &[String]) -> Result<Vec<RelKindId>> {
    let mut out = Vec::with_capacity(names.len());
    for n in names {
        let def = schema
            .relation_by_name(n)
            .ok_or_else(|| ElmdbError::new(ErrorCode::UnknownRelation, format!("unknown relation '{n}'")))?;
        out.push(def.id);
    }
    Ok(out)
}

fn resolve_field_def<'a>(
    schema: &'a CompiledSchema,
    active_type: Option<elmdb_engine::TypeId>,
    field: &str,
) -> Result<(FieldId, &'a FieldKind)> {
    if let Some(tid) = active_type {
        let tdef = schema
            .type_by_id(tid)
            .ok_or_else(|| ElmdbError::new(ErrorCode::UnknownType, format!("unknown type id {}", tid.0)))?;
        let fdef = tdef.field_by_name(field).ok_or_else(|| {
            ElmdbError::new(ErrorCode::UnknownField, format!("unknown field '{field}' on type '{}'", tdef.name))
        })?;
        return Ok((fdef.id, &fdef.kind));
    }
    let mut found: Option<(FieldId, &'a FieldKind)> = None;
    for include_archived in [false, true] {
        for tdef in schema.types.iter().filter(|t| t.archived == include_archived) {
            if let Some(fdef) = tdef.field_by_name(field) {
                match found {
                    None => found = Some((fdef.id, &fdef.kind)),
                    Some((prev_id, prev_kind)) => {
                        if prev_id != fdef.id || prev_kind != &fdef.kind {
                            return Err(ElmdbError::new(
                                ErrorCode::InvalidArgument,
                                format!(
                                    "field '{field}' is ambiguous across types (different field IDs or kinds); scope the query to a single type"
                                ),
                            ));
                        }
                    }
                }
            }
        }
        if found.is_some() {
            break;
        }
    }
    found.ok_or_else(|| ElmdbError::new(ErrorCode::UnknownField, format!("unknown field '{field}'")))
}

fn resolve_field_id(
    schema: &CompiledSchema,
    active_type: Option<elmdb_engine::TypeId>,
    field: &str,
) -> Result<FieldId> {
    Ok(resolve_field_def(schema, active_type, field)?.0)
}

fn resolve_index_id(schema: &CompiledSchema, active_type: Option<elmdb_engine::TypeId>, name: &str) -> Result<IndexId> {
    if let Some(tid) = active_type {
        return schema
            .indexes
            .iter()
            .find(|i| i.type_id == tid && i.name == name)
            .map(|i| i.id)
            .ok_or_else(|| ElmdbError::new(ErrorCode::UnknownIndex, format!("unknown index '{name}'")));
    }
    let matches: Vec<IndexId> = schema.indexes.iter().filter(|i| i.name == name).map(|i| i.id).collect();
    match matches.as_slice() {
        [id] => Ok(*id),
        [] => Err(ElmdbError::new(ErrorCode::UnknownIndex, format!("unknown index '{name}'"))),
        _ => Err(ElmdbError::new(
            ErrorCode::InvalidArgument,
            format!("index '{name}' is ambiguous across types; scope the query to a single type"),
        )),
    }
}

fn json_as_whole_i64(v: &serde_json::Value) -> Option<i64> {
    if let Some(i) = v.as_i64() {
        return Some(i);
    }
    let f = v.as_f64()?;
    if f.is_finite() && f.fract() == 0.0 && f >= (i64::MIN as f64) && f < (i64::MAX as f64) {
        Some(f as i64)
    } else {
        None
    }
}

/// Coerces a JSON filter/cursor literal according to the target field's `FieldKind`
/// so queries on `Timestamp`, `Ref`, and `Bytes` fields match stored `CanonicalValue`s cleanly.
fn coerce_json_for_field<F>(v: &serde_json::Value, kind: &FieldKind, resolve_ext_id: &mut F) -> Result<CanonicalValue>
where
    F: FnMut(&str) -> Result<Option<EntityKey>>,
{
    if v.is_null() {
        return Ok(CanonicalValue::Null);
    }
    match kind {
        FieldKind::Timestamp => {
            if let Some(ms) = json_as_whole_i64(v) {
                return Ok(CanonicalValue::Timestamp { ms, ns: 0 });
            }
            if let Some(obj) = v.as_object()
                && let Some(ms) = obj.get("ms").and_then(json_as_whole_i64)
            {
                let ns_raw = obj.get("ns").map_or(Some(0), json_as_whole_i64).ok_or_else(|| {
                    ElmdbError::new(ErrorCode::ValidationFailed, "timestamp ns must be a whole integer")
                })?;
                if !(0..1_000_000_000).contains(&ns_raw) {
                    return Err(ElmdbError::new(ErrorCode::ValidationFailed, "timestamp ns must be in 0..1e9"));
                }
                return Ok(CanonicalValue::Timestamp { ms, ns: ns_raw as u32 });
            }
            json_to_canonical(v)
        }
        FieldKind::Ref { .. } => {
            if let Some(ext_id) = v.as_str() {
                return match resolve_ext_id(ext_id)? {
                    Some(k) => Ok(CanonicalValue::EntityRef(k.0)),
                    None => Ok(CanonicalValue::DanglingRef(ext_id.to_owned())),
                };
            }
            json_to_canonical(v)
        }
        FieldKind::Bytes => {
            if let Some(arr) = v.as_array() {
                let mut bytes = Vec::with_capacity(arr.len());
                for item in arr {
                    let b = json_as_whole_i64(item).and_then(|n| u8::try_from(n).ok()).ok_or_else(|| {
                        ElmdbError::new(ErrorCode::ValidationFailed, "bytes filter elements must be 0..=255")
                    })?;
                    bytes.push(b);
                }
                return Ok(CanonicalValue::Bytes(bytes));
            }
            json_to_canonical(v)
        }
        _ => json_to_canonical(v),
    }
}

/// Parses a string-named `QuerySpec` into a typed integer-ID `LogicalPlan` once at the boundary!
pub fn compile_query_spec<F>(schema: &CompiledSchema, spec: &QuerySpec, mut resolve_ext_id: F) -> Result<LogicalPlan>
where
    F: FnMut(&str) -> Result<Option<EntityKey>>,
{
    let (mut active_type, mut node) = match &spec.source {
        QuerySource::Type { type_name, include_trashed } => {
            let td = schema
                .type_by_name_any(type_name)
                .ok_or_else(|| ElmdbError::new(ErrorCode::UnknownType, format!("unknown type '{type_name}'")))?;
            (Some(td.id), LogicalNode::Source(SourceSpec::Type { type_id: td.id, include_trashed: *include_trashed }))
        }
        QuerySource::Types { type_names, include_trashed } => {
            let mut tids = Vec::with_capacity(type_names.len());
            for tn in type_names {
                let td = schema
                    .type_by_name_any(tn)
                    .ok_or_else(|| ElmdbError::new(ErrorCode::UnknownType, format!("unknown type '{tn}'")))?;
                tids.push(td.id);
            }
            let single = if tids.len() == 1 { Some(tids[0]) } else { None };
            (single, LogicalNode::Source(SourceSpec::Types { type_ids: tids, include_trashed: *include_trashed }))
        }
        QuerySource::AllEntities { include_trashed } => {
            (None, LogicalNode::Source(SourceSpec::AllEntities { include_trashed: *include_trashed }))
        }
        QuerySource::Ids { ids } => {
            let mut keys = Vec::with_capacity(ids.len());
            for id in ids {
                if let Some(k) = resolve_ext_id(id)? {
                    keys.push(k);
                }
            }
            (None, LogicalNode::Source(SourceSpec::LiteralKeys(keys)))
        }
    };

    for step in &spec.steps {
        match step {
            QueryStep::Filter { expr } => {
                let compiled = compile_filter(schema, active_type, expr, &mut resolve_ext_id)?;
                node = LogicalNode::Filter { input: Box::new(node), expr: compiled };
            }
            QueryStep::Score { spec: score_spec } => {
                let compiled = compile_score(schema, active_type, score_spec, &mut resolve_ext_id)?;
                node = LogicalNode::Score { input: Box::new(node), spec: compiled };
            }
            QueryStep::Hops { rel_kinds, dir, min_hops, max_hops } => {
                let rks = resolve_rel_kinds(schema, rel_kinds)?;
                let d = parse_edge_dir(dir)?;
                active_type = None;
                node = LogicalNode::Traverse {
                    input: Box::new(node),
                    spec: TraverseSpec::Hops { rel_kinds: rks, dir: d, min_hops: *min_hops, max_hops: *max_hops },
                };
            }
            QueryStep::ShortestPath { rel_kinds, dir, target_id, max_hops } => {
                let rks = resolve_rel_kinds(schema, rel_kinds)?;
                let d = parse_edge_dir(dir)?;
                let target = resolve_ext_id(target_id)?.unwrap_or(EntityKey(0));
                active_type = None;
                node = LogicalNode::Traverse {
                    input: Box::new(node),
                    spec: TraverseSpec::ShortestPath { rel_kinds: rks, dir: d, target, max_hops: *max_hops },
                };
            }
            QueryStep::Distinct => {
                node = LogicalNode::Distinct { input: Box::new(node) };
            }
            QueryStep::OrderSlice { by, field, dir, locale, after_id, after_score, after_value, limit } => {
                let odir = parse_order_dir(dir)?;
                let mut order_field_kind: Option<&FieldKind> = None;
                let order = match by.as_str() {
                    "id" | "entityKey" => OrderSpec::EntityKey(odir),
                    "score" => OrderSpec::Score(odir),
                    "field" => {
                        let fname = field.as_deref().ok_or_else(|| {
                            ElmdbError::new(ErrorCode::InvalidArgument, "OrderSlice by='field' requires field")
                        })?;
                        let (fid, fkind) = resolve_field_def(schema, active_type, fname)?;
                        order_field_kind = Some(fkind);
                        OrderSpec::Field { field: fid, dir: odir, locale: locale.clone() }
                    }
                    other => {
                        return Err(ElmdbError::new(ErrorCode::InvalidArgument, format!("invalid order by '{other}'")));
                    }
                };
                let after = if let Some(aid) = after_id {
                    let key = resolve_ext_id(aid)?.ok_or_else(|| {
                        ElmdbError::new(ErrorCode::EntityNotFound, format!("cursor entity '{aid}' not found"))
                    })?;
                    match &order {
                        OrderSpec::EntityKey(_) => Some(Cursor::EntityKey(key)),
                        OrderSpec::Score(_) => Some(Cursor::Score {
                            score: after_score.ok_or_else(|| {
                                ElmdbError::new(ErrorCode::InvalidArgument, "score cursor requires afterScore")
                            })?,
                            key,
                        }),
                        OrderSpec::Field { .. } => {
                            let val = match (after_value.as_ref(), order_field_kind) {
                                (Some(v), Some(k)) => coerce_json_for_field(v, k, &mut resolve_ext_id)?,
                                (Some(v), None) => json_to_canonical(v)?,
                                (None, _) => CanonicalValue::Null,
                            };
                            Some(Cursor::Field { value: val, key })
                        }
                    }
                } else {
                    None
                };
                node = LogicalNode::OrderSlice { input: Box::new(node), order, after, limit: *limit };
            }
            QueryStep::GraphPpr { rel_kinds, weighted, seeds, damping, iterations, tolerance } => {
                let rks = resolve_rel_kinds(schema, rel_kinds)?;
                let mut resolved_seeds = Vec::with_capacity(seeds.len());
                for (id, w) in seeds {
                    if let Some(k) = resolve_ext_id(id)? {
                        resolved_seeds.push((k, *w));
                    }
                }
                node = LogicalNode::GraphAnalyze {
                    input: Box::new(node),
                    spec: GraphAnalyzeSpec::Ppr {
                        rel_kinds: rks,
                        weighted: *weighted,
                        seeds: resolved_seeds,
                        damping: *damping,
                        iterations: *iterations,
                        tolerance: *tolerance,
                    },
                };
            }
            QueryStep::GraphWeakComponents { rel_kinds } => {
                let rks = resolve_rel_kinds(schema, rel_kinds)?;
                node = LogicalNode::GraphAnalyze {
                    input: Box::new(node),
                    spec: GraphAnalyzeSpec::WeakComponents { rel_kinds: rks },
                };
            }
            QueryStep::GraphScc { rel_kinds } => {
                let rks = resolve_rel_kinds(schema, rel_kinds)?;
                node =
                    LogicalNode::GraphAnalyze { input: Box::new(node), spec: GraphAnalyzeSpec::Scc { rel_kinds: rks } };
            }
            QueryStep::GraphTopoSort { rel_kinds } => {
                let rks = resolve_rel_kinds(schema, rel_kinds)?;
                node = LogicalNode::GraphAnalyze {
                    input: Box::new(node),
                    spec: GraphAnalyzeSpec::TopoSort { rel_kinds: rks },
                };
            }
            QueryStep::GraphLeiden { rel_kinds, weighted, resolution, seed, iterations } => {
                let rks = resolve_rel_kinds(schema, rel_kinds)?;
                node = LogicalNode::GraphAnalyze {
                    input: Box::new(node),
                    spec: GraphAnalyzeSpec::Leiden {
                        rel_kinds: rks,
                        weighted: *weighted,
                        resolution: *resolution,
                        seed: *seed,
                        iterations: *iterations,
                    },
                };
            }
            QueryStep::GraphJaccard { rel_kinds, dir, seed_id, top_k } => {
                let rks = resolve_rel_kinds(schema, rel_kinds)?;
                let d = parse_edge_dir(dir)?;
                let seed = resolve_ext_id(seed_id)?.unwrap_or(EntityKey(0));
                node = LogicalNode::GraphAnalyze {
                    input: Box::new(node),
                    spec: GraphAnalyzeSpec::Jaccard { rel_kinds: rks, dir: d, seed, top_k: *top_k },
                };
            }
            QueryStep::GraphAdamicAdar { rel_kinds, dir, seed_id, top_k } => {
                let rks = resolve_rel_kinds(schema, rel_kinds)?;
                let d = parse_edge_dir(dir)?;
                let seed = resolve_ext_id(seed_id)?.unwrap_or(EntityKey(0));
                node = LogicalNode::GraphAnalyze {
                    input: Box::new(node),
                    spec: GraphAnalyzeSpec::AdamicAdar { rel_kinds: rks, dir: d, seed, top_k: *top_k },
                };
            }
        }
    }

    if let Some(term) = &spec.terminal {
        let terminal = match term {
            QueryTerminal::Ids => TerminalSpec::Ids,
            QueryTerminal::Rows { fields } => {
                let mask = match fields {
                    Some(names) => {
                        let mut fm = FieldMask::empty();
                        for n in names {
                            fm.insert(resolve_field_id(schema, active_type, n)?);
                        }
                        Some(fm)
                    }
                    None => None,
                };
                TerminalSpec::Rows { fields: mask }
            }
            QueryTerminal::Hits { fields, highlights } => {
                let mask = match fields {
                    Some(names) => {
                        let mut fm = FieldMask::empty();
                        for n in names {
                            fm.insert(resolve_field_id(schema, active_type, n)?);
                        }
                        Some(fm)
                    }
                    None => None,
                };
                TerminalSpec::Hits { fields: mask, highlights: *highlights }
            }
            QueryTerminal::Count => TerminalSpec::Count,
            QueryTerminal::Exists => TerminalSpec::Exists,
            QueryTerminal::Sum { field } => TerminalSpec::Sum(resolve_field_id(schema, active_type, field)?),
            QueryTerminal::MinMax { field } => TerminalSpec::MinMax(resolve_field_id(schema, active_type, field)?),
            QueryTerminal::GroupBy { field, counts_only } => TerminalSpec::GroupBy {
                field: resolve_field_id(schema, active_type, field)?,
                counts_only: *counts_only,
            },
            QueryTerminal::Degrees { rel_kinds, dir } => {
                TerminalSpec::Degrees { rel_kinds: resolve_rel_kinds(schema, rel_kinds)?, dir: parse_edge_dir(dir)? }
            }
        };
        node = LogicalNode::Terminal { input: Box::new(node), terminal };
    }

    Ok(LogicalPlan::new(node))
}

fn compile_filter<F>(
    schema: &CompiledSchema,
    active_type: Option<elmdb_engine::TypeId>,
    expr: &QueryFilter,
    resolve_ext_id: &mut F,
) -> Result<FilterExpr>
where
    F: FnMut(&str) -> Result<Option<EntityKey>>,
{
    Ok(match expr {
        QueryFilter::Eq { field, value } => {
            let (fid, fkind) = resolve_field_def(schema, active_type, field)?;
            FilterExpr::Eq(fid, coerce_json_for_field(value, fkind, resolve_ext_id)?)
        }
        QueryFilter::Ne { field, value } => {
            let (fid, fkind) = resolve_field_def(schema, active_type, field)?;
            FilterExpr::Ne(fid, coerce_json_for_field(value, fkind, resolve_ext_id)?)
        }
        QueryFilter::In { field, values } => {
            let (fid, fkind) = resolve_field_def(schema, active_type, field)?;
            let mut vs = Vec::with_capacity(values.len());
            for v in values {
                vs.push(coerce_json_for_field(v, fkind, resolve_ext_id)?);
            }
            FilterExpr::In(fid, vs)
        }
        QueryFilter::Range { field, low, high } => {
            let (fid, fkind) = resolve_field_def(schema, active_type, field)?;
            FilterExpr::Range {
                field: fid,
                low: match low {
                    Some((v, inc)) => Some((coerce_json_for_field(v, fkind, resolve_ext_id)?, *inc)),
                    None => None,
                },
                high: match high {
                    Some((v, inc)) => Some((coerce_json_for_field(v, fkind, resolve_ext_id)?, *inc)),
                    None => None,
                },
            }
        }
        QueryFilter::StartsWith { field, prefix } => {
            FilterExpr::StartsWith(resolve_field_id(schema, active_type, field)?, prefix.clone())
        }
        QueryFilter::Contains { field, needle } => {
            FilterExpr::Contains(resolve_field_id(schema, active_type, field)?, needle.clone())
        }
        QueryFilter::Regex { field, pattern } => {
            FilterExpr::Regex(resolve_field_id(schema, active_type, field)?, pattern.clone())
        }
        QueryFilter::IsNull { field } => FilterExpr::IsNull(resolve_field_id(schema, active_type, field)?),
        QueryFilter::IsPresent { field } => FilterExpr::IsPresent(resolve_field_id(schema, active_type, field)?),
        QueryFilter::HasLink { rel_kinds, dir, target_id } => {
            let target = match target_id {
                Some(id) => Some(resolve_ext_id(id)?.unwrap_or(EntityKey(0))),
                None => None,
            };
            FilterExpr::HasLink { rel_kinds: resolve_rel_kinds(schema, rel_kinds)?, dir: parse_edge_dir(dir)?, target }
        }
        QueryFilter::And { filters } => {
            let mut out = Vec::with_capacity(filters.len());
            for f in filters {
                out.push(compile_filter(schema, active_type, f, resolve_ext_id)?);
            }
            FilterExpr::And(out)
        }
        QueryFilter::Or { filters } => {
            let mut out = Vec::with_capacity(filters.len());
            for f in filters {
                out.push(compile_filter(schema, active_type, f, resolve_ext_id)?);
            }
            FilterExpr::Or(out)
        }
        QueryFilter::Not { filter } => {
            FilterExpr::Not(Box::new(compile_filter(schema, active_type, filter, resolve_ext_id)?))
        }
    })
}

fn compile_score<F>(
    schema: &CompiledSchema,
    active_type: Option<elmdb_engine::TypeId>,
    spec: &QueryScore,
    resolve_ext_id: &mut F,
) -> Result<ScoreSpec>
where
    F: FnMut(&str) -> Result<Option<EntityKey>>,
{
    Ok(match spec {
        QueryScore::Bm25f { index, query, require_all, prefix, phrase, top_k } => ScoreSpec::Bm25f {
            index_id: resolve_index_id(schema, active_type, index)?,
            query: query.clone(),
            require_all: *require_all,
            prefix: *prefix,
            phrase: phrase.clone(),
            top_k: *top_k,
        },
        QueryScore::VectorNear { index, vector, top_k } => ScoreSpec::VectorNear {
            index_id: resolve_index_id(schema, active_type, index)?,
            vector: vector.clone(),
            top_k: *top_k,
        },
        QueryScore::VectorLike { index, seed_id, top_k } => ScoreSpec::VectorLike {
            index_id: resolve_index_id(schema, active_type, index)?,
            seed: resolve_ext_id(seed_id)?.unwrap_or(EntityKey(0)),
            top_k: *top_k,
        },
        QueryScore::RankIndex { index } => {
            ScoreSpec::RankIndex { index_id: resolve_index_id(schema, active_type, index)? }
        }
        QueryScore::FuzzyLabel { field, query, max_distance } => ScoreSpec::FuzzyLabel {
            field: resolve_field_id(schema, active_type, field)?,
            query: query.clone(),
            max_distance: *max_distance,
        },
        QueryScore::RecencyDecay { field, now_ms, half_life_ms } => ScoreSpec::RecencyDecay {
            field: resolve_field_id(schema, active_type, field)?,
            now_ms: *now_ms,
            half_life_ms: *half_life_ms,
        },
        QueryScore::Fuse { inputs, method, rrf_k } => {
            let mut compiled = Vec::with_capacity(inputs.len());
            for (sub, w) in inputs {
                compiled.push((compile_score(schema, active_type, sub, resolve_ext_id)?, *w));
            }
            let fuse_method = match method.as_str() {
                "" | "rrf" | "Rrf" => FuseMethod::Rrf { k: rrf_k.unwrap_or(60.0) },
                "linear" | "linearNormalized" | "LinearNormalized" => FuseMethod::LinearNormalized,
                other => {
                    return Err(ElmdbError::new(ErrorCode::InvalidArgument, format!("invalid fuse method '{other}'")));
                }
            };
            ScoreSpec::Fuse { inputs: compiled, method: fuse_method }
        }
    })
}

/// Converts an internal integer-ID `QueryResult` into an external string-ID `QueryAnswer`,
/// using a per-response `EntityKey -> ext_id` cache so repeated `EntityRef` lookups do 1 LMDB read instead of N!
pub fn materialize_query_answer<F>(
    schema: &CompiledSchema,
    result: QueryResult,
    explain: Option<&ExplainPlan>,
    mut resolve_key: F,
) -> Result<QueryAnswer>
where
    F: FnMut(EntityKey) -> Result<Option<String>>,
{
    let mut key_cache: rustc_hash::FxHashMap<EntityKey, String> = rustc_hash::FxHashMap::default();
    let mut lookup = |k: EntityKey| -> Result<String> {
        if let Some(s) = key_cache.get(&k) {
            return Ok(s.clone());
        }
        let ext = resolve_key(k)?.unwrap_or_else(|| format!("__deleted:{}", k.0));
        key_cache.insert(k, ext.clone());
        Ok(ext)
    };

    let body = match result {
        QueryResult::Ids(keys) => {
            let mut ids = Vec::with_capacity(keys.len());
            for k in keys {
                ids.push(lookup(k)?);
            }
            QueryAnswerBody::Ids { ids }
        }
        QueryResult::Rows(rows) => {
            let mut out = Vec::with_capacity(rows.len());
            for r in rows {
                out.push(materialize_projected_row(schema, r, &mut lookup)?);
            }
            QueryAnswerBody::Rows { rows: out }
        }
        QueryResult::Hits(hs) => {
            let mut hits = Vec::with_capacity(hs.len());
            for h in hs {
                let row = h.row.map(|r| materialize_projected_row(schema, r, &mut lookup)).transpose()?;
                hits.push(Hit { id: lookup(h.key)?, score: h.score, row, highlights: h.highlights });
            }
            QueryAnswerBody::Hits { hits }
        }
        QueryResult::Count(count) => QueryAnswerBody::Count { count },
        QueryResult::Exists(exists) => QueryAnswerBody::Exists { exists },
        QueryResult::Sum(sum) => QueryAnswerBody::Sum { sum },
        QueryResult::MinMax { min, max } => QueryAnswerBody::MinMax {
            min: min.as_ref().map(|v| canonical_to_json_resolved(v, &mut lookup)).transpose()?,
            max: max.as_ref().map(|v| canonical_to_json_resolved(v, &mut lookup)).transpose()?,
        },
        QueryResult::GroupCounts(gc) => {
            let mut groups = Vec::with_capacity(gc.len());
            for (v, c) in gc {
                groups.push((canonical_to_json_resolved(&v, &mut lookup)?, c));
            }
            QueryAnswerBody::GroupCounts { groups }
        }
        QueryResult::Groups(gs) => {
            let mut groups = Vec::with_capacity(gs.len());
            for (v, ks) in gs {
                let mut ids = Vec::with_capacity(ks.len());
                for k in ks {
                    ids.push(lookup(k)?);
                }
                groups.push((canonical_to_json_resolved(&v, &mut lookup)?, ids));
            }
            QueryAnswerBody::Groups { groups }
        }
        QueryResult::Degrees(ds) => {
            let mut degrees = Vec::with_capacity(ds.len());
            for (k, d) in ds {
                degrees.push((lookup(k)?, d));
            }
            QueryAnswerBody::Degrees { degrees }
        }
        QueryResult::GraphGroups(gs) => {
            let mut groups = Vec::with_capacity(gs.len());
            for g in gs {
                let mut ids = Vec::with_capacity(g.len());
                for k in g {
                    ids.push(lookup(k)?);
                }
                groups.push(ids);
            }
            QueryAnswerBody::GraphGroups { groups }
        }
        QueryResult::GraphTopo(t) => {
            let mut order = Vec::with_capacity(t.order.len());
            for k in t.order {
                order.push(lookup(k)?);
            }
            QueryAnswerBody::GraphTopo { order, cycles: t.cycles }
        }
        QueryResult::GraphPpr(p) => {
            let mut scores = Vec::with_capacity(p.scores.len());
            for (k, score) in p.scores {
                scores.push(Hit { id: lookup(k)?, score, row: None, highlights: None });
            }
            QueryAnswerBody::GraphPpr { scores, converged: p.converged, iterations: p.iterations }
        }
    };

    let explain_json = explain
        .map(serde_json::to_value)
        .transpose()
        .map_err(|e| ElmdbError::new(ErrorCode::Internal, e.to_string()))?;

    Ok(QueryAnswer { body, explain: explain_json })
}

fn materialize_projected_row<F>(schema: &CompiledSchema, r: elmdb_query::ProjectedRow, lookup: &mut F) -> Result<Row>
where
    F: FnMut(EntityKey) -> Result<String>,
{
    let tdef = schema.type_by_id(r.type_id);
    let type_name = tdef.map(|t| t.name.clone()).unwrap_or_default();
    let mut fields = BTreeMap::new();
    for (fid, val) in &r.fields {
        let fname =
            tdef.and_then(|t| t.field_by_id(*fid)).map(|f| f.name.clone()).unwrap_or_else(|| format!("f{}", fid.0));
        fields.insert(fname, canonical_to_json_resolved(val, lookup)?);
    }
    Ok(Row { id: r.id, type_name, trashed: r.trashed, fields })
}

fn canonical_to_json_resolved<F>(v: &CanonicalValue, lookup: &mut F) -> Result<serde_json::Value>
where
    F: FnMut(EntityKey) -> Result<String>,
{
    Ok(match v {
        CanonicalValue::EntityRef(k) => serde_json::Value::String(lookup(EntityKey(*k))?),
        CanonicalValue::Array(xs) => {
            let mut out = Vec::with_capacity(xs.len());
            for x in xs {
                out.push(canonical_to_json_resolved(x, lookup)?);
            }
            serde_json::Value::Array(out)
        }
        CanonicalValue::Object(map) => {
            let mut out = serde_json::Map::new();
            for (k, val) in map {
                out.insert(k.clone(), canonical_to_json_resolved(val, lookup)?);
            }
            serde_json::Value::Object(out)
        }
        other => canonical_to_json(other),
    })
}
