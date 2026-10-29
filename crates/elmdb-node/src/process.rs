//! One elmdb binary per process. Claims are kernel-owned and disappear at process exit: an abstract Unix socket
//! on Linux, a receive right at a reserved task-local Mach name on macOS, and a named event on Windows. No account
//! database, home directory, temporary file, cleanup thread or /proc mount participates in claim acquisition.

use std::io;
use std::sync::Mutex;

use napi::Result;

use crate::Code;

#[cfg(target_os = "linux")]
type Claim = std::os::unix::net::UnixDatagram;
#[cfg(target_os = "macos")]
type Claim = MachClaim;
#[cfg(windows)]
type Claim = std::os::windows::io::OwnedHandle;
#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
type Claim = ();

#[cfg(target_os = "linux")]
fn claim_address() -> io::Result<std::os::unix::net::SocketAddr> {
    use sha2::{Digest, Sha256};
    use std::os::linux::net::SocketAddrExt;
    // AT_RANDOM is installed by the ELF loader and shared by every binary in this process. Hash its 16 bytes:
    // publishing them directly would expose random material also used by libc's stack protector. Including the PID
    // distinguishes forked children; fresh exec randomness distinguishes equal PIDs in containers sharing a netns.
    // SAFETY: getauxval has no pointer arguments. A nonzero AT_RANDOM points to 16 loader-owned bytes that remain
    // live for the process lifetime. No environment variable or /proc read supplies this address.
    let random = unsafe { libc::getauxval(libc::AT_RANDOM) } as *const u8;
    if random.is_null() {
        return Err(io::Error::other("the ELF loader did not supply process identity randomness"));
    }
    let mut digest = Sha256::new();
    digest.update(b"elmdb-binding-process-claim-v2");
    // SAFETY: the non-null loader-owned AT_RANDOM region contains exactly 16 readable bytes, as specified above.
    digest.update(unsafe { std::slice::from_raw_parts(random, 16) });
    digest.update(std::process::id().to_ne_bytes());
    let mut name = b"elmdb-binding-".to_vec();
    name.extend_from_slice(&digest.finalize());
    std::os::unix::net::SocketAddr::from_abstract_name(name)
}

#[cfg(target_os = "linux")]
fn acquire_claim() -> io::Result<Claim> {
    Claim::bind_addr(&claim_address()?).map_err(|error| {
        if error.kind() == io::ErrorKind::AddrInUse {
            io::Error::new(io::ErrorKind::WouldBlock, "the process is already claimed")
        } else {
            error
        }
    })
}

#[cfg(target_os = "macos")]
#[derive(Debug)]
struct MachClaim(u32);

#[cfg(target_os = "macos")]
unsafe extern "C" {
    static mach_task_self_: u32;
    fn mach_port_allocate_name(task: u32, right: u32, name: u32) -> i32;
    fn mach_port_mod_refs(task: u32, name: u32, right: u32, delta: i32) -> i32;
}

#[cfg(target_os = "macos")]
impl Drop for MachClaim {
    fn drop(&mut self) {
        // SAFETY: this claim uniquely owns one receive right in the current task. Reducing its reference count
        // destroys that right. Static successful claims are never dropped; this also permits isolated unit tests.
        unsafe { mach_port_mod_refs(mach_task_self_, self.0, 1, -1) };
    }
}

#[cfg(target_os = "macos")]
fn acquire_claim() -> io::Result<Claim> {
    // Reserve one name in this task's IPC table, not launchd's service namespace. The nonzero generation is a legal
    // Mach name. All copies must use exactly this name, even if occupied by an unrelated component: fail closed
    // rather than selecting another name and admitting two LMDB implementations. No filesystem or entitlement to
    // register a launchd service is needed. The slot and receive right are reclaimed with the task, including SIGKILL.
    const NAME: u32 = 0x000e_1d03;
    const RECEIVE: u32 = 1;
    // SAFETY: mach_task_self names our live task; RECEIVE and NAME describe a new task-owned receive right.
    match unsafe { mach_port_allocate_name(mach_task_self_, RECEIVE, NAME) } {
        0 => Ok(MachClaim(NAME)),
        libc::KERN_NAME_EXISTS => Err(io::Error::new(io::ErrorKind::WouldBlock, "the process claim name is occupied")),
        status => Err(io::Error::other(format!("cannot reserve the Mach process claim (status {status})"))),
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn acquire_claim() -> io::Result<Claim> {
    Err(io::Error::new(io::ErrorKind::Unsupported, "process claims require Linux, macOS or Windows"))
}

#[cfg(windows)]
fn acquire_event(name: &str) -> io::Result<Claim> {
    use std::ffi::c_void;
    use std::os::windows::io::FromRawHandle;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn CreateEventW(attributes: *const c_void, manual: i32, initial: i32, name: *const u16) -> *mut c_void;
        fn GetLastError() -> u32;
    }
    let name: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
    // SAFETY: name is a terminated wide string alive for the call. Null attributes create a non-inheritable
    // handle; the event is never waited on or signalled, so only its atomic named-object creation matters.
    let raw = unsafe { CreateEventW(std::ptr::null(), 1, 0, name.as_ptr()) };
    // SAFETY: GetLastError has no preconditions and must immediately follow CreateEventW.
    let error = unsafe { GetLastError() };
    if raw.is_null() {
        return Err(io::Error::from_raw_os_error(error as i32));
    }
    // SAFETY: CreateEventW returned an owned valid handle, transferred exactly once to OwnedHandle.
    let handle = unsafe { Claim::from_raw_handle(raw) };
    if error == 183 {
        return Err(io::Error::new(io::ErrorKind::WouldBlock, "the process is already claimed"));
    }
    Ok(handle)
}

#[cfg(windows)]
fn acquire_claim() -> io::Result<Claim> {
    acquire_event(&format!("Local\\elmdb-binding-{}", std::process::id()))
}

/// Keep the accepted Windows image and its static claim mapped after its first worker unloads it. A leaked claim
/// handle alone survives DLL unloading, but a reloaded image would lose the static and reject its own old claim.
#[cfg(windows)]
fn pin_module() -> io::Result<()> {
    use std::ffi::c_void;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetModuleHandleExW(flags: u32, address: *const u16, module: *mut *mut c_void) -> i32;
    }
    const FROM_ADDRESS: u32 = 0x0000_0004;
    const PIN: u32 = 0x0000_0001;
    let mut module = std::ptr::null_mut();
    // SAFETY: FROM_ADDRESS interprets the second argument as an address, not a wide string. This function's
    // address belongs to the currently executing binding image; module points to valid output storage. PIN retains
    // that image until process termination, so no FreeLibrary call is required or appropriate for this handle.
    let success = unsafe { GetModuleHandleExW(FROM_ADDRESS | PIN, pin_module as *const () as *const u16, &mut module) };
    if success == 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
}

/// Claims the process before this binary opens any database. Each independently loaded binary owns its own claim mutex
/// and independently acquired OS handle, so only one wins even when first opens race across worker threads.
pub fn claim() -> Result<(), Code> {
    claim_process().map_err(|(code, message)| napi::Error::new(Code(code), message))
}

/// `claim`, answering a refusal as its error code and message
fn claim_process() -> std::result::Result<(), (&'static str, String)> {
    static CLAIMED: Mutex<Option<Claim>> = Mutex::new(None);
    match claim_once(&CLAIMED, || {
        let claim = acquire_claim()?;
        #[cfg(windows)]
        pin_module()?;
        Ok::<_, io::Error>(claim)
    }) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
            let mine = process_path::get_dylib_path()
                .map_or_else(|| "an unknown file".to_owned(), |p| p.display().to_string());
            #[cfg(target_os = "macos")]
            let message = format!(
                "The Mach process claim name is occupied, so {mine} can't open a database. Another elmdb binary or an unrelated component may own it. Ensure only one elmdb package is loaded; if the collision persists, report the conflicting component"
            );
            #[cfg(not(target_os = "macos"))]
            let message = format!(
                "Another elmdb binary already claimed this process, so {mine} can't open a database. Load one elmdb package"
            );
            Err(("ALREADY_LOADED", message))
        }
        Err(error) => Err((
            "IO",
            format!("Cannot claim this process: {error}. Ensure the operating system permits the process claim"),
        )),
    }
}

/// Cache only successful claims. An acquisition failure must remain retryable without reloading the binding.
fn claim_once<T>(slot: &Mutex<Option<T>>, acquire: impl FnOnce() -> io::Result<T>) -> io::Result<()> {
    let mut held = slot.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if held.is_none() {
        *held = Some(acquire()?);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failed_claims_are_retryable_and_success_is_cached() {
        let slot = Mutex::new(None::<()>);
        assert!(claim_once(&slot, || Err(io::Error::other("transient failure"))).is_err());
        claim_once(&slot, || Ok(())).unwrap();
        claim_once(&slot, || panic!("a successful claim must not be reacquired")).unwrap();
    }

    /// The claimer half of the next test, run as a child of this test binary: a claim is the process's once made
    #[test]
    fn child_claimer() {
        if std::env::var("ELMDB_CHILD_CLAIM").is_err() {
            return;
        }
        let held = acquire_claim().unwrap();
        let (code, message) = claim_process().unwrap_err();
        assert_eq!(code, "ALREADY_LOADED", "{message}");
        drop(held);
        claim_process().unwrap();
        claim_process().unwrap();
        println!("claimed");
    }

    #[test]
    fn a_claim_held_elsewhere_in_the_process_is_refused_as_already_loaded_and_made_once_free() {
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "process::tests::child_claimer", "--nocapture", "--test-threads=1"])
            .env("ELMDB_CHILD_CLAIM", "1")
            .output()
            .unwrap();
        assert!(
            out.status.success() && String::from_utf8_lossy(&out.stdout).contains("claimed"),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    #[cfg(unix)]
    #[test]
    fn independent_kernel_claims_conflict_and_are_reclaimed_on_close() {
        let first = acquire_claim().unwrap();
        assert_eq!(acquire_claim().unwrap_err().kind(), io::ErrorKind::WouldBlock);
        drop(first);
        acquire_claim().unwrap();
    }
}
