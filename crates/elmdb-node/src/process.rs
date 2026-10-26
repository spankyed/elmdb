//! One elmdb binary per process. Each binary statically links its own LMDB, and two copies of LMDB share no locks, so
//! a second binary opening a database in a process that already loaded one could corrupt it. The first open claims the
//! process in its environment (`ELMDB_BINDING`), which every thread and every loaded copy share, and an open from
//! another binary throws, naming both files. Two binaries' first opens racing on two threads can both pass; one loaded
//! after the other opened, the case a bundled copy beside an installed one makes, can't.

use napi::Result;

use crate::Code;

/// This binary's file
fn own_file() -> String {
    process_path::get_dylib_path().map_or_else(|| "(an unknown file)".to_owned(), |p| p.display().to_string())
}

/// The variable a process's environment names its elmdb binary in
const VARIABLE: &str = "ELMDB_BINDING";

/// Claims the process for this binary, or refuses when another binary claimed it. The claim names the process, since a
/// child inherits its parent's environment and may load a binary of its own
fn claim_process(mine: &str) -> Result<(), Code> {
    static CLAIMED: std::sync::Once = std::sync::Once::new();
    let pid = std::process::id().to_string();
    if let Some(held) = std::env::var(VARIABLE).ok()
        && let Some((owner, theirs)) = held.split_once(':')
        && owner == pid
        && theirs != mine
    {
        return Err(already_loaded(theirs, mine));
    }
    CLAIMED.call_once(|| {
        // SAFETY: the variable is set once per binary, on its first open, before any database of this binary exists;
        // a thread reading the environment at that moment is the hazard `set_var` names, and nothing elmdb runs does
        unsafe { std::env::set_var(VARIABLE, format!("{pid}:{mine}")) };
    });
    Ok(())
}

fn already_loaded(theirs: &str, mine: &str) -> napi::Error<Code> {
    napi::Error::new(
        Code("ALREADY_LOADED"),
        format!(
            "Another elmdb binary is loaded in this process ({theirs}), so this one ({mine}) can't open a database: two \
             copies of LMDB in one process share no locks. Load one elmdb package"
        ),
    )
}

/// Claims the process for this binary on an open, or refuses one from a second binary
pub fn claim() -> Result<(), Code> {
    claim_process(&own_file())
}
