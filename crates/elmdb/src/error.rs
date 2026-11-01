//! Every failure is an [`Error`] with a code from one enum, [`ErrorCode`], whose TypeScript the build generates. A code
//! keeps its meaning for good: a new kind of failure takes a new code.

use serde::{Deserialize, Serialize};

/// What went wrong, as a stable code the binding hands JavaScript as `error.code`
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "codegen", derive(ts_rs::TS))]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ErrorCode {
    /// A file operation failed: creating the directory, or LMDB reading or writing its files
    Io,
    /// LMDB returned an error no other code names
    Lmdb,
    /// The map is full and couldn't be grown
    MapFull,
    /// Every reader slot in the environment is taken
    ReadersFull,
    /// LMDB found the files damaged
    Corrupted,
    /// The database is already open in this process; LMDB allows one open per environment per process
    AlreadyOpen,
    /// The database was closed
    Closed,
    /// A write to a database opened read-only
    ReadOnly,
    /// A read-only open of a directory that holds no database
    NoDatabase,
    /// The schema has problems, every one listed in the message by its path
    SchemaInvalid,
    /// The given schema changes the stored one in a way opening refuses, such as a short code prefix
    SchemaChangeRefused,
    /// Another process changed the stored schema; close and reopen the handle before reading or writing
    SchemaChanged,
    /// The files hold something other than an elmdb database
    NotADatabase,
    /// The database is in a format this version doesn't read
    WrongFormat,
    /// A value isn't in the value spec (`docs/architecture/format.md`, Values)
    ValueInvalid,
    /// A batch or plan doesn't decode, or holds an operation this version doesn't know
    BatchInvalid,
    /// A type the schema doesn't declare
    UnknownType,
    /// A field its type doesn't declare, on a type that refuses additional fields
    UnknownField,
    /// A relation kind the schema doesn't declare
    UnknownRelationKind,
    /// A role the schema doesn't declare
    UnknownRole,
    /// An id no entity has
    NoSuchEntity,
    /// An id no relation has
    NoSuchRelation,
    /// A create given an id an entity already has
    IdInUse,
    /// An id that names another type than the one given
    TypeMismatch,
    /// A write to a field only the engine writes
    ReservedField,
    /// A type name the engine's own ids use
    ReservedType,
    /// A list write to a field whose value isn't a list
    NotAList,
    /// A created row lacking a field its type requires, or a delete of one
    RequiredField,
    /// A value its field's declared type doesn't take
    FieldType,
    /// A regular expression that doesn't parse, or uses what Rust's `regex` doesn't hold (look-around, backreferences)
    PatternInvalid,
    /// A short code that isn't the type's `<prefix>-<n>`, or one an entity can't take
    InvalidShortCode,
    /// A short code another entity holds
    ShortCodeInUse,
    /// A link from an entity to itself
    SelfLink,
    /// A link between types its kind's endpoints don't allow
    Endpoint,
    /// A link past its kind's cardinality
    Cardinality,
    /// A link that would close a cycle through an acyclic kind
    Cycle,
    /// A write that would give two entities the same values in a unique index
    Unique,
    /// An operation a unit can't hold, made while one is open, or a second unit begun
    UnitOpen,
    /// A search naming an index its type doesn't have, or none on a type with no text index or with several; a
    /// `ranked` step naming no rank index
    UnknownIndex,
    /// A read of an index still being built, which has nothing to answer from in its place: a rank index
    IndexBuilding,
    /// A wait for the index builds that ended with one or more failed: `indexStatus()` says why each did
    IndexBuildFailed,
    /// A durable operation whose transaction committed and whose sync then failed: the data is committed, and not yet known to
    /// be on disk. Retrying the operation would apply it twice
    NotDurable,
    /// A second elmdb binary loaded into a process that already loaded one: two copies of LMDB in one process share no
    /// locks
    AlreadyLoaded,
    /// A relation of a `unique` kind moved onto two ends another edge of its kind already joins
    DuplicateEdge,
    /// A short code or label numbering whose next number would pass 2^53 - 1, the largest JavaScript holds exactly
    NumbersExhausted,
    /// The initial watch listener panicked; its registration was removed
    CallbackPanicked,
    /// The initial watch read panicked; its registration was removed
    WatchPanicked,
    /// Closing a database from its own watch callback would wait on the callback's operation
    CallbackActive,
}

impl ErrorCode {
    /// The code as JavaScript sees it, `SCREAMING_SNAKE_CASE`
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Io => "IO",
            Self::Lmdb => "LMDB",
            Self::MapFull => "MAP_FULL",
            Self::ReadersFull => "READERS_FULL",
            Self::Corrupted => "CORRUPTED",
            Self::AlreadyOpen => "ALREADY_OPEN",
            Self::Closed => "CLOSED",
            Self::ReadOnly => "READ_ONLY",
            Self::NoDatabase => "NO_DATABASE",
            Self::SchemaInvalid => "SCHEMA_INVALID",
            Self::SchemaChangeRefused => "SCHEMA_CHANGE_REFUSED",
            Self::SchemaChanged => "SCHEMA_CHANGED",
            Self::NotADatabase => "NOT_A_DATABASE",
            Self::WrongFormat => "WRONG_FORMAT",
            Self::ValueInvalid => "VALUE_INVALID",
            Self::BatchInvalid => "BATCH_INVALID",
            Self::UnknownType => "UNKNOWN_TYPE",
            Self::UnknownField => "UNKNOWN_FIELD",
            Self::UnknownRelationKind => "UNKNOWN_RELATION_KIND",
            Self::UnknownRole => "UNKNOWN_ROLE",
            Self::NoSuchEntity => "NO_SUCH_ENTITY",
            Self::NoSuchRelation => "NO_SUCH_RELATION",
            Self::IdInUse => "ID_IN_USE",
            Self::TypeMismatch => "TYPE_MISMATCH",
            Self::ReservedField => "RESERVED_FIELD",
            Self::ReservedType => "RESERVED_TYPE",
            Self::NotAList => "NOT_A_LIST",
            Self::RequiredField => "REQUIRED_FIELD",
            Self::FieldType => "FIELD_TYPE",
            Self::PatternInvalid => "PATTERN_INVALID",
            Self::InvalidShortCode => "INVALID_SHORT_CODE",
            Self::ShortCodeInUse => "SHORT_CODE_IN_USE",
            Self::SelfLink => "SELF_LINK",
            Self::Endpoint => "ENDPOINT",
            Self::Cardinality => "CARDINALITY",
            Self::Cycle => "CYCLE",
            Self::Unique => "UNIQUE",
            Self::UnitOpen => "UNIT_OPEN",
            Self::UnknownIndex => "UNKNOWN_INDEX",
            Self::IndexBuilding => "INDEX_BUILDING",
            Self::IndexBuildFailed => "INDEX_BUILD_FAILED",
            Self::NotDurable => "NOT_DURABLE",
            Self::AlreadyLoaded => "ALREADY_LOADED",
            Self::DuplicateEdge => "DUPLICATE_EDGE",
            Self::NumbersExhausted => "NUMBERS_EXHAUSTED",
            Self::CallbackPanicked => "CALLBACK_PANICKED",
            Self::WatchPanicked => "WATCH_PANICKED",
            Self::CallbackActive => "CALLBACK_ACTIVE",
        }
    }
}

/// A failure: its code, and a message that says what failed and, where there is one, the fix
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{code}: {message}", code = .code.as_str())]
pub struct Error {
    pub code: ErrorCode,
    pub message: String,
}

impl Error {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self { code, message: message.into() }
    }
}

pub type Result<T> = std::result::Result<T, Error>;

impl From<heed::Error> for Error {
    fn from(error: heed::Error) -> Self {
        use heed::MdbError;
        let code = match &error {
            heed::Error::Io(_) => ErrorCode::Io,
            heed::Error::EnvAlreadyOpened => ErrorCode::AlreadyOpen,
            heed::Error::Mdb(MdbError::MapFull) => ErrorCode::MapFull,
            heed::Error::Mdb(MdbError::ReadersFull) => ErrorCode::ReadersFull,
            heed::Error::Mdb(MdbError::Corrupted | MdbError::PageNotFound | MdbError::Invalid) => ErrorCode::Corrupted,
            _ => ErrorCode::Lmdb,
        };
        Self::new(code, error.to_string())
    }
}

impl From<std::io::Error> for Error {
    fn from(error: std::io::Error) -> Self {
        Self::new(ErrorCode::Io, error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn as_str_is_the_serialized_name() {
        for code in [
            ErrorCode::Io,
            ErrorCode::Lmdb,
            ErrorCode::MapFull,
            ErrorCode::ReadersFull,
            ErrorCode::Corrupted,
            ErrorCode::AlreadyOpen,
            ErrorCode::Closed,
            ErrorCode::ReadOnly,
            ErrorCode::NoDatabase,
            ErrorCode::SchemaInvalid,
            ErrorCode::SchemaChangeRefused,
            ErrorCode::NotADatabase,
            ErrorCode::WrongFormat,
            ErrorCode::ValueInvalid,
            ErrorCode::BatchInvalid,
            ErrorCode::UnknownType,
            ErrorCode::UnknownField,
            ErrorCode::UnknownRelationKind,
            ErrorCode::UnknownRole,
            ErrorCode::NoSuchEntity,
            ErrorCode::NoSuchRelation,
            ErrorCode::IdInUse,
            ErrorCode::TypeMismatch,
            ErrorCode::ReservedField,
            ErrorCode::ReservedType,
            ErrorCode::NotAList,
            ErrorCode::RequiredField,
            ErrorCode::FieldType,
            ErrorCode::PatternInvalid,
            ErrorCode::InvalidShortCode,
            ErrorCode::ShortCodeInUse,
            ErrorCode::SelfLink,
            ErrorCode::Endpoint,
            ErrorCode::Cardinality,
            ErrorCode::Cycle,
            ErrorCode::Unique,
            ErrorCode::UnitOpen,
            ErrorCode::UnknownIndex,
            ErrorCode::IndexBuilding,
            ErrorCode::IndexBuildFailed,
            ErrorCode::CallbackPanicked,
            ErrorCode::WatchPanicked,
            ErrorCode::CallbackActive,
        ] {
            assert_eq!(serde_json::to_value(code).unwrap(), code.as_str());
        }
    }
}
