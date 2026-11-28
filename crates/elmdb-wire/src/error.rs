use elmdb_storage::StorageError;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Single authoritative error-code taxonomy shared by Rust, N-API, and `@elmdb/ears`.
///
/// Zero renames across layers: `UNKNOWN_TYPE` in Rust is `UNKNOWN_TYPE` in TypeScript.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
#[repr(u16)]
pub enum ErrorCode {
    InvalidArgument = 1,
    InvalidWireFrame = 2,
    InvalidSchema = 3,
    SchemaChanged = 4,
    UnknownType = 5,
    TypeArchived = 6,
    UnknownField = 7,
    UnknownRelation = 8,
    UnknownIndex = 9,
    EntityNotFound = 10,
    EntityAlreadyExists = 11,
    EntityTrashed = 12,
    EntityNotTrashed = 13,
    RelationNotFound = 14,
    RelationAlreadyExists = 15,
    ValidationFailed = 16,
    UniqueViolation = 17,
    CardinalityViolation = 18,
    CycleDetected = 19,
    Referenced = 20,
    EndpointMismatch = 21,
    HookAborted = 22,
    HookCycleLimit = 23,
    AsyncCallbackForbidden = 24,
    IndexBuildFailed = 25,
    IndexNotReady = 26,
    ResourceLimit = 27,
    Cancelled = 28,
    StorageFull = 29,
    Corrupted = 30,
    DurabilityFailed = 31,
    Poisoned = 32,
    Io = 33,
    Internal = 34,
    Closed = 35,
    ReadOnly = 36,
    NoDatabase = 37,
    DatabaseLocked = 38,
    AlreadyExists = 39,
    Panicked = 40,
    AlreadyLoaded = 41,
}

impl ErrorCode {
    pub const ALL: &'static [Self] = &[
        Self::InvalidArgument,
        Self::InvalidWireFrame,
        Self::InvalidSchema,
        Self::SchemaChanged,
        Self::UnknownType,
        Self::TypeArchived,
        Self::UnknownField,
        Self::UnknownRelation,
        Self::UnknownIndex,
        Self::EntityNotFound,
        Self::EntityAlreadyExists,
        Self::EntityTrashed,
        Self::EntityNotTrashed,
        Self::RelationNotFound,
        Self::RelationAlreadyExists,
        Self::ValidationFailed,
        Self::UniqueViolation,
        Self::CardinalityViolation,
        Self::CycleDetected,
        Self::Referenced,
        Self::EndpointMismatch,
        Self::HookAborted,
        Self::HookCycleLimit,
        Self::AsyncCallbackForbidden,
        Self::IndexBuildFailed,
        Self::IndexNotReady,
        Self::ResourceLimit,
        Self::Cancelled,
        Self::StorageFull,
        Self::Corrupted,
        Self::DurabilityFailed,
        Self::Poisoned,
        Self::Io,
        Self::Internal,
        Self::Closed,
        Self::ReadOnly,
        Self::NoDatabase,
        Self::DatabaseLocked,
        Self::AlreadyExists,
        Self::Panicked,
        Self::AlreadyLoaded,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InvalidArgument => "INVALID_ARGUMENT",
            Self::InvalidWireFrame => "INVALID_WIRE_FRAME",
            Self::InvalidSchema => "INVALID_SCHEMA",
            Self::SchemaChanged => "SCHEMA_CHANGED",
            Self::UnknownType => "UNKNOWN_TYPE",
            Self::TypeArchived => "TYPE_ARCHIVED",
            Self::UnknownField => "UNKNOWN_FIELD",
            Self::UnknownRelation => "UNKNOWN_RELATION",
            Self::UnknownIndex => "UNKNOWN_INDEX",
            Self::EntityNotFound => "ENTITY_NOT_FOUND",
            Self::EntityAlreadyExists => "ENTITY_ALREADY_EXISTS",
            Self::EntityTrashed => "ENTITY_TRASHED",
            Self::EntityNotTrashed => "ENTITY_NOT_TRASHED",
            Self::RelationNotFound => "RELATION_NOT_FOUND",
            Self::RelationAlreadyExists => "RELATION_ALREADY_EXISTS",
            Self::ValidationFailed => "VALIDATION_FAILED",
            Self::UniqueViolation => "UNIQUE_VIOLATION",
            Self::CardinalityViolation => "CARDINALITY_VIOLATION",
            Self::CycleDetected => "CYCLE_DETECTED",
            Self::Referenced => "REFERENCED",
            Self::EndpointMismatch => "ENDPOINT_MISMATCH",
            Self::HookAborted => "HOOK_ABORTED",
            Self::HookCycleLimit => "HOOK_CYCLE_LIMIT",
            Self::AsyncCallbackForbidden => "ASYNC_CALLBACK_FORBIDDEN",
            Self::IndexBuildFailed => "INDEX_BUILD_FAILED",
            Self::IndexNotReady => "INDEX_NOT_READY",
            Self::ResourceLimit => "RESOURCE_LIMIT",
            Self::Cancelled => "CANCELLED",
            Self::StorageFull => "STORAGE_FULL",
            Self::Corrupted => "CORRUPTED",
            Self::DurabilityFailed => "DURABILITY_FAILED",
            Self::Poisoned => "POISONED",
            Self::Io => "IO",
            Self::Internal => "INTERNAL",
            Self::Closed => "CLOSED",
            Self::ReadOnly => "READ_ONLY",
            Self::NoDatabase => "NO_DATABASE",
            Self::DatabaseLocked => "DATABASE_LOCKED",
            Self::AlreadyExists => "ALREADY_EXISTS",
            Self::Panicked => "PANICKED",
            Self::AlreadyLoaded => "ALREADY_LOADED",
        }
    }

    /// Generates the exact TypeScript union declaration for `@elmdb/ears` / `elmdb-node`.
    pub fn ts_type_definition() -> String {
        let mut out = String::from("// Generated from crates/elmdb-wire/src/error.rs — do not edit by hand.\n");
        out.push_str("export type ErrorCode =\n");
        for code in Self::ALL {
            out.push_str(&format!("  | '{}'\n", code.as_str()));
        }
        out.push_str(";\n");
        out
    }
}

/// Structured engine error carrying one `ErrorCode`, human message, optional JSON path,
/// and optional `declaredBy` attribution.
#[derive(Debug, Clone, PartialEq, Eq, Error, Serialize, Deserialize)]
#[error("{code:?}: {message}")]
pub struct ElmdbError {
    pub code: ErrorCode,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub declared_by: Option<String>,
}

impl ElmdbError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self { code, message: message.into(), path: None, declared_by: None }
    }

    pub fn with_path(mut self, path: impl Into<String>) -> Self {
        self.path = Some(path.into());
        self
    }

    pub fn with_declared_by(mut self, declared_by: impl Into<String>) -> Self {
        self.declared_by = Some(declared_by.into());
        self
    }

    /// Returns `true` strictly for LMDB `MDB_MAP_FULL` (never `DbsFull`, `TxnFull`, or vector generation exhaustion).
    pub fn is_map_full(&self) -> bool {
        self.code == ErrorCode::StorageFull && self.message.starts_with("storage map is full")
    }
}

impl From<StorageError> for ElmdbError {
    fn from(err: StorageError) -> Self {
        let code = match &err {
            StorageError::NotFound => ErrorCode::EntityNotFound,
            StorageError::KeyExist => ErrorCode::EntityAlreadyExists,
            StorageError::MapFull { .. } | StorageError::DbsFull { .. } | StorageError::TxnFull => {
                ErrorCode::StorageFull
            }
            StorageError::DatabaseLocked(_) => ErrorCode::DatabaseLocked,
            StorageError::ReadOnly => ErrorCode::ReadOnly,
            StorageError::ReadersFull => ErrorCode::ResourceLimit,
            StorageError::BadValSize { .. } => ErrorCode::InvalidArgument,
            StorageError::Corrupted(_) => ErrorCode::Corrupted,
            StorageError::Poisoned(_) => ErrorCode::Poisoned,
            StorageError::DurabilityFailed(_) => ErrorCode::DurabilityFailed,
            StorageError::Io(_) => ErrorCode::Io,
            StorageError::MapResized
            | StorageError::InvalidSubDb { .. }
            | StorageError::SavepointMismatch { .. }
            | StorageError::InvalidState(_) => ErrorCode::Internal,
        };
        Self::new(code, err.to_string())
    }
}

pub type Result<T> = std::result::Result<T, ElmdbError>;

#[cfg(test)]
mod tests {
    use super::ErrorCode;

    #[test]
    fn error_code_as_str_matches_protocol_names() {
        assert_eq!(ErrorCode::InvalidArgument.as_str(), "INVALID_ARGUMENT");
        assert_eq!(ErrorCode::AlreadyLoaded.as_str(), "ALREADY_LOADED");
        let ts = ErrorCode::ts_type_definition();
        for code in ErrorCode::ALL {
            assert!(!code.as_str().is_empty());
            assert!(ts.contains(&format!("'{}'", code.as_str())));
        }
    }
}
