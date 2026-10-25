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
        ] {
            assert_eq!(serde_json::to_value(code).unwrap(), code.as_str());
        }
    }
}
