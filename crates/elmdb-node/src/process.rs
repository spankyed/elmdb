//! One elmdb binary per process. Each binary statically links its own LMDB, and two copies of LMDB share no locks, so
//! a second binary opening a database in a process that already loaded one could corrupt it. The first open claims the
//! process under a global symbol, and an open from another binary throws, naming both files.

use napi::bindgen_prelude::*;

use crate::Code;

const KEY: &str = "elmdb.binding";

/// This binary's file
fn own_file() -> String {
    process_path::get_dylib_path().map_or_else(|| "(an unknown file)".to_owned(), |p| p.display().to_string())
}

pub fn claim(env: &Env) -> Result<(), Code> {
    let global = env.get_global().map_err(status)?;
    let key = env.symbol_for(KEY).map_err(status)?;
    let mine = own_file();
    let holder: Unknown = global.get_property(key).map_err(status)?;
    match holder.get_type().map_err(status)? {
        ValueType::Undefined => {
            let value = env.create_string(&mine).map_err(status)?;
            let mut global = global;
            global.set_property(key, value).map_err(status)
        }
        ValueType::String => {
            let theirs =
                holder.coerce_to_string().map_err(status)?.into_utf8().map_err(status)?.into_owned().map_err(status)?;
            if theirs == mine {
                Ok(())
            } else {
                Err(napi::Error::new(
                    Code("ALREADY_LOADED"),
                    format!(
                        "Another elmdb binary is loaded in this process ({theirs}), so this one ({mine}) can't open a \
                         database: two copies of LMDB in one process share no locks. Load one elmdb package"
                    ),
                ))
            }
        }
        _ => Err(napi::Error::new(
            Code("ALREADY_LOADED"),
            format!("globalThis[Symbol.for('{KEY}')] holds something else"),
        )),
    }
}

fn status<S: AsRef<str>>(error: napi::Error<S>) -> napi::Error<Code> {
    napi::Error::new(Code("LMDB"), error.reason.clone())
}
