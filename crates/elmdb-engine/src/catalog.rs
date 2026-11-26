//! Parse-don't-validate schema compiler and O(1) metadata evolution.
//!
//! - Assigns strictly monotonic, never-reused `TypeId`, `FieldId`, `RelKindId`, `IndexId`.
//! - Maps each live index to a reusable `SubDbSlot(u16)` drawn from `SubDbPool` (never leaks DBIs).
//! - Keeps removed types marked `archived: true` so uninstalled pack data remains readable/purgeable
//!   while refusing new writes with `ErrorCode::TypeArchived`.

use std::collections::{BTreeMap, BTreeSet};

use elmdb_storage::{SubDbPool, SubDbSlot};
use elmdb_wire::{ElmdbError, ErrorCode, Result};
use serde::{Deserialize, Serialize};

use crate::ids::{FieldId, FieldMask, IndexId, RelKindId, TypeId};

/// Referential-integrity action enforced atomically via `ref_in` when a referenced entity is destroyed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum OnDeletePolicy {
    /// Default: refuse `destroy` with `ErrorCode::Referenced` if any live entity references it.
    #[default]
    Restrict,
    /// Atomically clear/null the referencing field on `destroy` (valid only on optional/nullable refs).
    SetNull,
    /// Atomically rewrite `EntityRef(key)` -> `DanglingRef(ext_id)` via `ref_in` before `destroy`.
    AllowDangling,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum RawFieldKind {
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
        on_delete: OnDeletePolicy,
    },
    List {
        of: Box<RawFieldKind>,
    },
    Object,
    Any,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RawField {
    #[serde(flatten)]
    pub kind: RawFieldKind,
    #[serde(default)]
    pub optional: bool,
    #[serde(default)]
    pub nullable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum RawIndexKind {
    Value {
        fields: Vec<String>,
        #[serde(default)]
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
        quantization: String,
    },
    Rank {
        relations: Vec<String>,
        #[serde(default)]
        weighted: bool,
    },
}

fn default_vec_quant() -> String {
    "sq8".to_owned()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RawIndex {
    #[serde(flatten)]
    pub kind: RawIndexKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RawType {
    #[serde(default)]
    pub fields: BTreeMap<String, RawField>,
    #[serde(default)]
    pub indexes: BTreeMap<String, RawIndex>,
    #[serde(default)]
    pub additional_fields: bool,
    #[serde(default)]
    pub short_code: Option<String>,
    #[serde(default)]
    pub change_order: bool,
    #[serde(default)]
    pub declared_by: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Cardinality {
    OneToOne,
    OneToMany,
    ManyToOne,
    #[default]
    ManyToMany,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RawEndpoint {
    pub source: String,
    pub target: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RawRelation {
    #[serde(default)]
    pub cardinality: Cardinality,
    #[serde(default)]
    pub acyclic: bool,
    #[serde(default)]
    pub weighted: bool,
    #[serde(default)]
    pub endpoints: Vec<RawEndpoint>,
    #[serde(default)]
    pub declared_by: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RawSchema {
    #[serde(default)]
    pub types: BTreeMap<String, RawType>,
    #[serde(default)]
    pub relations: BTreeMap<String, RawRelation>,
}

// ---------------------------------------------------------------------------
// Compiled, invariant-bearing schema (integer catalog IDs, persisted in `meta`)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum FieldKind {
    String,
    Number,
    Integer,
    Int64,
    Boolean,
    Bytes,
    Timestamp,
    Enum(Vec<String>),
    Ref { to: Vec<TypeId>, on_delete: OnDeletePolicy },
    List(Box<FieldKind>),
    Object,
    Any,
}

impl FieldKind {
    pub fn contains_ref(&self) -> bool {
        match self {
            Self::Ref { .. } => true,
            Self::List(inner) => inner.contains_ref(),
            _ => false,
        }
    }

    pub fn ref_policy(&self) -> Option<(&[TypeId], OnDeletePolicy)> {
        match self {
            Self::Ref { to, on_delete } => Some((to.as_slice(), *on_delete)),
            Self::List(inner) => inner.ref_policy(),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FieldDef {
    pub id: FieldId,
    pub name: String,
    pub kind: FieldKind,
    pub optional: bool,
    pub nullable: bool,
    pub archived: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum IndexKind {
    Value { fields: Vec<FieldId>, unique: bool },
    Order { field: FieldId, locale: String },
    Text { fields: Vec<(FieldId, u16)> },
    Trigram { fields: Vec<FieldId> },
    Vector { fields: Vec<FieldId>, dims: u32, quantization: String },
    Rank { relations: Vec<RelKindId>, weighted: bool },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexDef {
    pub id: IndexId,
    pub name: String,
    pub type_id: TypeId,
    /// Physical LMDB sub-DB slot drawn from `SubDbPool` (`p_000`..`p_479`).
    pub slot: u16,
    pub kind: IndexKind,
}

impl IndexDef {
    pub fn subdb_slot(&self) -> SubDbSlot {
        SubDbSlot::new(self.slot).expect("compiled index slot in bounds")
    }

    pub fn field_mask(&self) -> FieldMask {
        let mut mask = FieldMask::empty();
        match &self.kind {
            IndexKind::Value { fields, .. } | IndexKind::Trigram { fields } | IndexKind::Vector { fields, .. } => {
                for &f in fields {
                    mask.insert(f);
                }
            }
            IndexKind::Order { field, .. } => mask.insert(*field),
            IndexKind::Text { fields } => {
                for &(f, _) in fields {
                    mask.insert(f);
                }
            }
            IndexKind::Rank { .. } => {}
        }
        mask
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TypeDef {
    pub id: TypeId,
    pub name: String,
    pub archived: bool,
    pub additional_fields: bool,
    pub short_code: Option<String>,
    pub change_order: bool,
    pub declared_by: Option<String>,
    pub next_field_id: u16,
    pub fields: Vec<FieldDef>,
}

impl TypeDef {
    pub fn field_by_name(&self, name: &str) -> Option<&FieldDef> {
        self.fields.iter().find(|f| !f.archived && f.name == name)
    }

    pub fn field_by_id(&self, id: FieldId) -> Option<&FieldDef> {
        self.fields.iter().find(|f| f.id == id)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelKindDef {
    pub id: RelKindId,
    pub name: String,
    pub archived: bool,
    pub cardinality: Cardinality,
    pub acyclic: bool,
    pub weighted: bool,
    pub endpoints: Vec<(TypeId, TypeId)>,
    pub declared_by: Option<String>,
}

/// Slots released or added during a schema transition so the commit pipeline can O(1)-truncate
/// retired sub-DBs and schedule builds for new indexes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SchemaDiff {
    pub added_indexes: Vec<IndexId>,
    pub retired_slots: Vec<SubDbSlot>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompiledSchema {
    pub epoch: u64,
    pub next_type_id: u16,
    pub next_rel_kind_id: u16,
    pub next_index_id: u32,
    pub types: Vec<TypeDef>,
    pub relations: Vec<RelKindDef>,
    pub indexes: Vec<IndexDef>,
}

impl CompiledSchema {
    pub fn empty() -> Self {
        Self {
            epoch: 0,
            next_type_id: 1,
            next_rel_kind_id: 1,
            next_index_id: 1,
            types: Vec::new(),
            relations: Vec::new(),
            indexes: Vec::new(),
        }
    }

    pub fn type_by_name(&self, name: &str) -> Option<&TypeDef> {
        self.types.iter().find(|t| !t.archived && t.name == name)
    }

    pub fn type_by_name_any(&self, name: &str) -> Option<&TypeDef> {
        self.types.iter().find(|t| t.name == name)
    }

    pub fn type_by_id(&self, id: TypeId) -> Option<&TypeDef> {
        self.types.iter().find(|t| t.id == id)
    }

    pub fn relation_by_name(&self, name: &str) -> Option<&RelKindDef> {
        self.relations.iter().find(|r| !r.archived && r.name == name)
    }

    pub fn relation_by_id(&self, id: RelKindId) -> Option<&RelKindDef> {
        self.relations.iter().find(|r| r.id == id)
    }

    pub fn indexes_for_type(&self, type_id: TypeId) -> impl Iterator<Item = &IndexDef> {
        self.indexes.iter().filter(move |idx| idx.type_id == type_id)
    }

    pub fn index_by_id(&self, id: IndexId) -> Option<&IndexDef> {
        self.indexes.iter().find(|idx| idx.id == id)
    }

    /// Compiles `raw` against an optional previous schema + `SubDbPool`, preserving monotonic IDs,
    /// archiving removed types/fields/relations, allocating pooled slots for new/changed indexes,
    /// and returning every diagnostic on invalid input.
    pub fn compile(
        raw: &RawSchema,
        prev: Option<&CompiledSchema>,
        pool: &mut SubDbPool,
    ) -> Result<(CompiledSchema, SchemaDiff)> {
        let mut errors: Vec<String> = Vec::new();
        let mut next_type_id = prev.map_or(1, |p| p.next_type_id);
        let mut next_rel_kind_id = prev.map_or(1, |p| p.next_rel_kind_id);
        let mut next_index_id = prev.map_or(1, |p| p.next_index_id);

        // Pass 1: Validate type names and assign/preserve TypeIds.
        let mut type_name_to_id: BTreeMap<String, TypeId> = BTreeMap::new();
        let mut types: Vec<TypeDef> = prev
            .map(|p| {
                p.types
                    .iter()
                    .cloned()
                    .map(|mut t| {
                        t.archived = true;
                        t
                    })
                    .collect()
            })
            .unwrap_or_default();

        for (name, raw_ty) in &raw.types {
            if name.is_empty() || name.contains('-') || name.len() > 64 {
                errors.push(format!("types.{name}: invalid type name (must be 1..=64 chars, no '-')"));
                continue;
            }
            if let Some(existing) = types.iter_mut().find(|t| t.name == *name) {
                existing.archived = false;
                existing.additional_fields = raw_ty.additional_fields;
                existing.short_code = raw_ty.short_code.clone();
                existing.change_order = raw_ty.change_order;
                existing.declared_by = raw_ty.declared_by.clone();
                type_name_to_id.insert(name.clone(), existing.id);
            } else {
                let id = TypeId(next_type_id);
                next_type_id = next_type_id
                    .checked_add(1)
                    .ok_or_else(|| ElmdbError::new(ErrorCode::InvalidSchema, "TypeId u16 space exhausted"))?;
                types.push(TypeDef {
                    id,
                    name: name.clone(),
                    archived: false,
                    additional_fields: raw_ty.additional_fields,
                    short_code: raw_ty.short_code.clone(),
                    change_order: raw_ty.change_order,
                    declared_by: raw_ty.declared_by.clone(),
                    next_field_id: 1,
                    fields: Vec::new(),
                });
                type_name_to_id.insert(name.clone(), id);
            }
        }

        // Pass 2: Compile fields per type (validating ref targets & onDelete rules).
        for (type_name, raw_ty) in &raw.types {
            let Some(&type_id) = type_name_to_id.get(type_name) else {
                continue;
            };
            let tdef = types.iter_mut().find(|t| t.id == type_id).expect("type exists");
            for f in &mut tdef.fields {
                f.archived = true;
            }
            for (fname, rfield) in &raw_ty.fields {
                if fname.is_empty() || fname.len() > 128 {
                    errors.push(format!("types.{type_name}.fields.{fname}: invalid field name"));
                    continue;
                }
                let kind = match compile_field_kind(
                    &rfield.kind,
                    &type_name_to_id,
                    rfield.optional || rfield.nullable,
                    &format!("types.{type_name}.fields.{fname}"),
                    &mut errors,
                ) {
                    Some(k) => k,
                    None => continue,
                };
                if let Some(existing) = tdef.fields.iter_mut().find(|f| f.name == *fname) {
                    if !existing.archived && existing.kind != kind {
                        errors.push(format!(
                            "types.{type_name}.fields.{fname}: incompatible field kind change on existing field"
                        ));
                        continue;
                    }
                    if existing.archived && existing.kind != kind {
                        // Previously archived field revived with a new kind -> allocate a fresh FieldId
                        // and retire the old archived name to avoid collision.
                        let old_id = existing.id.0;
                        existing.name = format!("__archived_{old_id}_{fname}");
                        let fid = FieldId(tdef.next_field_id);
                        tdef.next_field_id = tdef
                            .next_field_id
                            .checked_add(1)
                            .ok_or_else(|| ElmdbError::new(ErrorCode::InvalidSchema, "FieldId u16 space exhausted"))?;
                        tdef.fields.push(FieldDef {
                            id: fid,
                            name: fname.clone(),
                            kind,
                            optional: rfield.optional,
                            nullable: rfield.nullable,
                            archived: false,
                        });
                    } else {
                        existing.kind = kind;
                        existing.optional = rfield.optional;
                        existing.nullable = rfield.nullable;
                        existing.archived = false;
                    }
                } else {
                    let fid = FieldId(tdef.next_field_id);
                    tdef.next_field_id = tdef
                        .next_field_id
                        .checked_add(1)
                        .ok_or_else(|| ElmdbError::new(ErrorCode::InvalidSchema, "FieldId u16 space exhausted"))?;
                    tdef.fields.push(FieldDef {
                        id: fid,
                        name: fname.clone(),
                        kind,
                        optional: rfield.optional,
                        nullable: rfield.nullable,
                        archived: false,
                    });
                }
            }
        }

        // Pass 3: Compile relations.
        let mut rel_name_to_id: BTreeMap<String, RelKindId> = BTreeMap::new();
        let mut relations: Vec<RelKindDef> = prev
            .map(|p| {
                p.relations
                    .iter()
                    .cloned()
                    .map(|mut r| {
                        r.archived = true;
                        r
                    })
                    .collect()
            })
            .unwrap_or_default();

        for (rname, rrel) in &raw.relations {
            if rname.is_empty() || rname.len() > 128 {
                errors.push(format!("relations.{rname}: invalid relation kind name"));
                continue;
            }
            let mut endpoints = Vec::with_capacity(rrel.endpoints.len());
            for (idx, ep) in rrel.endpoints.iter().enumerate() {
                match (type_name_to_id.get(&ep.source), type_name_to_id.get(&ep.target)) {
                    (Some(&src), Some(&dst)) => endpoints.push((src, dst)),
                    _ => errors.push(format!(
                        "relations.{rname}.endpoints[{idx}]: unknown endpoint type '{}' -> '{}'",
                        ep.source, ep.target
                    )),
                }
            }
            if let Some(existing) = relations.iter_mut().find(|r| r.name == *rname) {
                existing.archived = false;
                existing.cardinality = rrel.cardinality;
                existing.acyclic = rrel.acyclic;
                existing.weighted = rrel.weighted;
                existing.endpoints = endpoints;
                existing.declared_by = rrel.declared_by.clone();
                rel_name_to_id.insert(rname.clone(), existing.id);
            } else {
                let id = RelKindId(next_rel_kind_id);
                next_rel_kind_id = next_rel_kind_id
                    .checked_add(1)
                    .ok_or_else(|| ElmdbError::new(ErrorCode::InvalidSchema, "RelKindId u16 space exhausted"))?;
                relations.push(RelKindDef {
                    id,
                    name: rname.clone(),
                    archived: false,
                    cardinality: rrel.cardinality,
                    acyclic: rrel.acyclic,
                    weighted: rrel.weighted,
                    endpoints,
                    declared_by: rrel.declared_by.clone(),
                });
                rel_name_to_id.insert(rname.clone(), id);
            }
        }

        // Pass 4: Compile indexes (validate before mutating SubDbPool!).
        let mut compiled_index_specs: Vec<(TypeId, String, IndexKind)> = Vec::new();
        for (type_name, raw_ty) in &raw.types {
            let Some(&type_id) = type_name_to_id.get(type_name) else {
                continue;
            };
            let tdef = types.iter().find(|t| t.id == type_id).expect("type exists");
            for (iname, ridx) in &raw_ty.indexes {
                let path = format!("types.{type_name}.indexes.{iname}");
                if iname.is_empty() || iname.len() > 128 {
                    errors.push(format!("{path}: invalid index name"));
                    continue;
                }
                let kind = match &ridx.kind {
                    RawIndexKind::Value { fields, unique } => {
                        if fields.is_empty() {
                            errors.push(format!("{path}: value index requires at least one field"));
                            continue;
                        }
                        let mut fids = Vec::with_capacity(fields.len());
                        for f in fields {
                            match tdef.field_by_name(f) {
                                Some(fd) => fids.push(fd.id),
                                None => errors.push(format!("{path}: unknown field '{f}'")),
                            }
                        }
                        IndexKind::Value { fields: fids, unique: *unique }
                    }
                    RawIndexKind::Order { field, locale } => {
                        if locale.is_empty() || locale.len() > 64 {
                            errors.push(format!("{path}: order index requires a non-empty locale"));
                            continue;
                        }
                        match tdef.field_by_name(field) {
                            Some(fd) => IndexKind::Order { field: fd.id, locale: locale.clone() },
                            None => {
                                errors.push(format!("{path}: unknown field '{field}'"));
                                continue;
                            }
                        }
                    }
                    RawIndexKind::Text { fields } => {
                        if fields.is_empty() {
                            errors.push(format!("{path}: text index requires at least one field"));
                            continue;
                        }
                        let mut fids = Vec::with_capacity(fields.len());
                        for (f, w) in fields {
                            match tdef.field_by_name(f) {
                                Some(fd) => fids.push((fd.id, (*w).max(1))),
                                None => errors.push(format!("{path}: unknown field '{f}'")),
                            }
                        }
                        IndexKind::Text { fields: fids }
                    }
                    RawIndexKind::Trigram { fields } => {
                        if fields.is_empty() {
                            errors.push(format!("{path}: trigram index requires at least one field"));
                            continue;
                        }
                        let mut fids = Vec::with_capacity(fields.len());
                        for f in fields {
                            match tdef.field_by_name(f) {
                                Some(fd) => fids.push(fd.id),
                                None => errors.push(format!("{path}: unknown field '{f}'")),
                            }
                        }
                        IndexKind::Trigram { fields: fids }
                    }
                    RawIndexKind::Vector { fields, dims, quantization } => {
                        if fields.is_empty() {
                            errors.push(format!("{path}: vector index requires at least one field"));
                            continue;
                        }
                        if *dims == 0 || *dims > 16_384 {
                            errors.push(format!("{path}: dims must be 1..=16384"));
                            continue;
                        }
                        if !matches!(quantization.as_str(), "sq8" | "f16" | "f32") {
                            errors.push(format!("{path}: quantization must be sq8, f16, or f32"));
                            continue;
                        }
                        let mut fids = Vec::with_capacity(fields.len());
                        for f in fields {
                            match tdef.field_by_name(f) {
                                Some(fd) => fids.push(fd.id),
                                None => errors.push(format!("{path}: unknown field '{f}'")),
                            }
                        }
                        IndexKind::Vector { fields: fids, dims: *dims, quantization: quantization.clone() }
                    }
                    RawIndexKind::Rank { relations: rel_names, weighted } => {
                        if rel_names.is_empty() {
                            errors.push(format!("{path}: rank index requires at least one relation"));
                            continue;
                        }
                        let mut rids = Vec::with_capacity(rel_names.len());
                        for rn in rel_names {
                            match rel_name_to_id.get(rn) {
                                Some(&rid) => rids.push(rid),
                                None => errors.push(format!("{path}: unknown relation '{rn}'")),
                            }
                        }
                        IndexKind::Rank { relations: rids, weighted: *weighted }
                    }
                };
                compiled_index_specs.push((type_id, iname.clone(), kind));
            }
        }

        if !errors.is_empty() {
            return Err(ElmdbError::new(ErrorCode::InvalidSchema, errors.join("; ")));
        }

        // Reconcile indexes against `prev`: recycle retired slots BEFORE allocating new ones
        // so replacing an index at pool capacity (480/480) never spuriously fails with DbsFull.
        let mut working_pool = pool.clone();
        let mut diff = SchemaDiff::default();
        let mut kept_prev_ids: BTreeSet<IndexId> = BTreeSet::new();
        let mut matched_specs: Vec<Option<IndexDef>> = Vec::with_capacity(compiled_index_specs.len());

        for (type_id, name, kind) in &compiled_index_specs {
            let matching_prev = prev.and_then(|p| {
                p.indexes.iter().find(|old| old.type_id == *type_id && old.name == *name && old.kind == *kind)
            });
            if let Some(old) = matching_prev {
                kept_prev_ids.insert(old.id);
                matched_specs.push(Some(old.clone()));
            } else {
                matched_specs.push(None);
            }
        }

        if let Some(p) = prev {
            for old in &p.indexes {
                if !kept_prev_ids.contains(&old.id) {
                    let slot = old.subdb_slot();
                    working_pool.recycle(slot)?;
                    diff.retired_slots.push(slot);
                }
            }
        }

        let mut indexes = Vec::with_capacity(compiled_index_specs.len());
        for ((type_id, name, kind), kept) in compiled_index_specs.into_iter().zip(matched_specs) {
            if let Some(old) = kept {
                indexes.push(old);
            } else {
                let slot = working_pool.allocate()?;
                let id = IndexId(next_index_id);
                next_index_id = next_index_id
                    .checked_add(1)
                    .ok_or_else(|| ElmdbError::new(ErrorCode::InvalidSchema, "IndexId u32 space exhausted"))?;
                indexes.push(IndexDef { id, name, type_id, slot: slot.raw(), kind });
                diff.added_indexes.push(id);
            }
        }

        *pool = working_pool;
        let epoch = prev.map_or(1, |p| p.epoch.saturating_add(1));

        Ok((CompiledSchema { epoch, next_type_id, next_rel_kind_id, next_index_id, types, relations, indexes }, diff))
    }

    /// O(1) metadata rename of a field on an active type (0 rows rewritten, 0 indexes rebuilt).
    pub fn rename_field(&self, type_name: &str, old_field: &str, new_field: &str) -> Result<Self> {
        if new_field.is_empty() || new_field.len() > 128 {
            return Err(ElmdbError::new(ErrorCode::InvalidArgument, "invalid new field name"));
        }
        let mut next = self.clone();
        let ty = next
            .types
            .iter_mut()
            .find(|t| !t.archived && t.name == type_name)
            .ok_or_else(|| ElmdbError::new(ErrorCode::UnknownType, format!("unknown type '{type_name}'")))?;
        if ty.fields.iter().any(|f| f.name == new_field) {
            return Err(ElmdbError::new(
                ErrorCode::InvalidSchema,
                format!("field '{new_field}' already exists (or is archived) on '{type_name}'"),
            ));
        }
        let f = ty.fields.iter_mut().find(|f| !f.archived && f.name == old_field).ok_or_else(|| {
            ElmdbError::new(ErrorCode::UnknownField, format!("unknown field '{old_field}' on '{type_name}'"))
        })?;
        f.name = new_field.to_owned();
        next.epoch = next.epoch.saturating_add(1);
        Ok(next)
    }

    /// O(1) metadata rename of a relation kind (0 edges rewritten).
    pub fn rename_relation(&self, old_rel: &str, new_rel: &str) -> Result<Self> {
        if new_rel.is_empty() || new_rel.len() > 128 {
            return Err(ElmdbError::new(ErrorCode::InvalidArgument, "invalid new relation name"));
        }
        let mut next = self.clone();
        if next.relations.iter().any(|r| r.name == new_rel) {
            return Err(ElmdbError::new(
                ErrorCode::InvalidSchema,
                format!("relation '{new_rel}' already exists (or is archived)"),
            ));
        }
        let rel = next
            .relations
            .iter_mut()
            .find(|r| !r.archived && r.name == old_rel)
            .ok_or_else(|| ElmdbError::new(ErrorCode::UnknownRelation, format!("unknown relation '{old_rel}'")))?;
        rel.name = new_rel.to_owned();
        next.epoch = next.epoch.saturating_add(1);
        Ok(next)
    }

    /// O(1) metadata rename of an entity type (0 rows rewritten; external IDs stay immutable by default).
    pub fn rename_type(&self, old_type: &str, new_type: &str) -> Result<Self> {
        if new_type.is_empty() || new_type.contains('-') || new_type.len() > 64 {
            return Err(ElmdbError::new(ErrorCode::InvalidArgument, "invalid new type name"));
        }
        let mut next = self.clone();
        if next.types.iter().any(|t| t.name == new_type) {
            return Err(ElmdbError::new(
                ErrorCode::InvalidSchema,
                format!("type '{new_type}' already exists (or is archived)"),
            ));
        }
        let ty = next
            .types
            .iter_mut()
            .find(|t| !t.archived && t.name == old_type)
            .ok_or_else(|| ElmdbError::new(ErrorCode::UnknownType, format!("unknown type '{old_type}'")))?;
        ty.name = new_type.to_owned();
        next.epoch = next.epoch.saturating_add(1);
        Ok(next)
    }
}

fn compile_field_kind(
    raw: &RawFieldKind,
    types: &BTreeMap<String, TypeId>,
    can_be_null_or_absent: bool,
    path: &str,
    errors: &mut Vec<String>,
) -> Option<FieldKind> {
    match raw {
        RawFieldKind::String => Some(FieldKind::String),
        RawFieldKind::Number => Some(FieldKind::Number),
        RawFieldKind::Integer => Some(FieldKind::Integer),
        RawFieldKind::Int64 => Some(FieldKind::Int64),
        RawFieldKind::Boolean => Some(FieldKind::Boolean),
        RawFieldKind::Bytes => Some(FieldKind::Bytes),
        RawFieldKind::Timestamp => Some(FieldKind::Timestamp),
        RawFieldKind::Enum { values } => {
            if values.is_empty() {
                errors.push(format!("{path}: enum must declare at least one value"));
                return None;
            }
            Some(FieldKind::Enum(values.clone()))
        }
        RawFieldKind::Ref { to, on_delete } => {
            if to.is_empty() {
                errors.push(format!("{path}: ref must declare at least one target type"));
                return None;
            }
            if *on_delete == OnDeletePolicy::SetNull && !can_be_null_or_absent {
                errors.push(format!("{path}: onDelete=setNull requires optional:true or nullable:true"));
                return None;
            }
            let mut ids = Vec::with_capacity(to.len());
            for target in to {
                match types.get(target) {
                    Some(&tid) => ids.push(tid),
                    None => errors.push(format!("{path}: ref target type '{target}' does not exist")),
                }
            }
            Some(FieldKind::Ref { to: ids, on_delete: *on_delete })
        }
        RawFieldKind::List { of } => {
            // Inside a list, SetNull removes the destroyed element cleanly without requiring nullable!
            let inner = compile_field_kind(of, types, true, &format!("{path}.of"), errors)?;
            Some(FieldKind::List(Box::new(inner)))
        }
        RawFieldKind::Object => Some(FieldKind::Object),
        RawFieldKind::Any => Some(FieldKind::Any),
    }
}
