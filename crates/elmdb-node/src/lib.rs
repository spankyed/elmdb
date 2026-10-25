//! The Node binding: elmdb's API for JavaScript, over N-API. Every failure throws an `Error` whose `code` is an
//! `ErrorCode`.

use napi::bindgen_prelude::*;
use napi_derive::napi;

/// An elmdb code as the status of a JavaScript error, which N-API hands JavaScript as `error.code`
pub struct Code(&'static str);

impl AsRef<str> for Code {
    fn as_ref(&self) -> &str {
        self.0
    }
}

/// An elmdb failure as JavaScript sees it: `error.code` is the elmdb code, `error.message` its message
fn to_js(error: elmdb::Error) -> napi::Error<Code> {
    napi::Error::new(Code(error.code.as_str()), error.message)
}

#[napi(object)]
pub struct OpenOptions {
    pub read_only: Option<bool>,
}

/// An open database: one directory holding one LMDB environment
#[napi]
pub struct Database {
    inner: elmdb::Database,
}

#[napi]
impl Database {
    /// Opens the database at `path`, creating it unless `readOnly`
    #[napi(factory)]
    pub fn open(path: String, options: Option<OpenOptions>) -> Result<Self, Code> {
        let options = elmdb::OpenOptions { read_only: options.and_then(|o| o.read_only).unwrap_or(false) };
        elmdb::Database::open(path, options).map(|inner| Self { inner }).map_err(to_js)
    }

    #[napi(getter)]
    pub fn path(&self) -> String {
        self.inner.path().display().to_string()
    }

    #[napi(getter)]
    pub fn read_only(&self) -> bool {
        self.inner.is_read_only()
    }

    #[napi(getter)]
    pub fn is_open(&self) -> bool {
        self.inner.is_open()
    }

    /// Releases the environment; closing a closed database does nothing
    #[napi]
    pub fn close(&mut self) {
        self.inner.close();
    }
}
