//! The boot a record was written in: an identity the machine loses when it stops and no clock makes
//! (`docs/roadmap/synced-commit.md`, The design). A record of another boot than this one was written before the machine
//! last stopped, so what was written after it and not synced may be gone.
//!
//! Linux names each boot in `/proc/sys/kernel/random/boot_id`, and Apple's systems in `kern.bootsessionuuid`. Windows
//! names none, so the first process to ask writes a random value in a registry key made volatile, which Windows keeps
//! in memory only: gone when the machine stops, and at a logoff too, which then reads as another boot. Where none can
//! be had the boot is none, which is another boot than any: a process's death there costs the commits after its last
//! sync, as a loss would, and opens no torn file.

use sha2::{Digest, Sha256};

/// A boot's identity, as a checksum of what the system names it; all zeros is none
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Boot([u8; 32]);

impl Boot {
    pub(super) fn none() -> Self {
        Self([0; 32])
    }

    /// The boot whose system's name for it is `name`
    pub(super) fn of(name: &[u8]) -> Self {
        Self(Sha256::digest(name).into())
    }

    pub(super) fn bytes(&self) -> &[u8; 32] {
        &self.0
    }

    pub(super) fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Whether both are the one boot. None is no boot, the same as none
    pub(super) fn same(&self, other: &Self) -> bool {
        *self != Self::none() && self == other
    }
}

/// The boot this process runs in
pub(super) fn this_boot() -> Boot {
    #[cfg(test)]
    if NO_BOOT.load(std::sync::atomic::Ordering::Relaxed) {
        return Boot::none();
    }
    named().map_or_else(Boot::none, |name| Boot::of(&name))
}

/// A test's stand-in for a system that names no boot, for the process it is set in
#[cfg(test)]
pub(crate) static NO_BOOT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

#[cfg(target_os = "linux")]
fn named() -> Option<Vec<u8>> {
    let name = std::fs::read("/proc/sys/kernel/random/boot_id").ok()?;
    (!name.trim_ascii().is_empty()).then_some(name)
}

#[cfg(target_os = "macos")]
fn named() -> Option<Vec<u8>> {
    let mut bytes = [0u8; 64];
    let mut len = bytes.len();
    // SAFETY: the name is a NUL-terminated C string, `bytes` holds `len` bytes, which sysctlbyname writes no more
    // than and sets to what it wrote, and no new value is given
    let answered = unsafe {
        libc::sysctlbyname(
            c"kern.bootsessionuuid".as_ptr(),
            bytes.as_mut_ptr().cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    let name = bytes.get(..len)?.split(|byte| *byte == 0).next()?;
    (answered == 0 && !name.is_empty()).then(|| name.to_vec())
}

#[cfg(windows)]
fn named() -> Option<Vec<u8>> {
    use windows_sys::Win32::Foundation::ERROR_SUCCESS;
    use windows_sys::Win32::System::Registry::{
        HKEY, HKEY_CURRENT_USER, KEY_READ, KEY_WRITE, REG_BINARY, REG_CREATED_NEW_KEY, REG_OPTION_VOLATILE,
        RegCloseKey, RegCreateKeyExW, RegQueryValueExW, RegSetValueExW,
    };
    let path: Vec<u16> = "Software\\elmdb-boot\0".encode_utf16().collect();
    let name: Vec<u16> = "boot\0".encode_utf16().collect();
    let mut key: HKEY = std::ptr::null_mut();
    let mut disposition = 0u32;
    // SAFETY: both strings are NUL-terminated UTF-16 that outlive the call, no class or security attributes are
    // given, and `key` and `disposition` are written by the call alone
    let created = unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            path.as_ptr(),
            0,
            std::ptr::null(),
            REG_OPTION_VOLATILE,
            KEY_READ | KEY_WRITE,
            std::ptr::null(),
            &mut key,
            &mut disposition,
        )
    };
    if created != ERROR_SUCCESS {
        return None;
    }
    let read = || {
        let (mut bytes, mut len, mut kind) = ([0u8; 16], 16u32, 0u32);
        // SAFETY: `key` is open, the name is NUL-terminated, and `bytes` holds the `len` bytes the call may write
        let answered =
            unsafe { RegQueryValueExW(key, name.as_ptr(), std::ptr::null(), &mut kind, bytes.as_mut_ptr(), &mut len) };
        (answered == ERROR_SUCCESS && kind == REG_BINARY && len == 16).then_some(bytes)
    };
    let write = || {
        let mut bytes = [0u8; 16];
        getrandom::fill(&mut bytes).ok()?;
        // SAFETY: `key` is open for writing, the name is NUL-terminated, and `bytes` holds the 16 bytes given
        let written = unsafe { RegSetValueExW(key, name.as_ptr(), 0, REG_BINARY, bytes.as_ptr(), 16) };
        (written == ERROR_SUCCESS).then_some(())
    };
    let named = if disposition == REG_CREATED_NEW_KEY {
        // The one process to make the key this boot names the boot
        write().and_then(|()| read())
    } else {
        // Another made it, and writes its value just after: waited for, and written here only where that process
        // died between the two, the value read back being whichever was written last
        let mut found = None;
        for _ in 0..100 {
            found = read();
            if found.is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        found.or_else(|| write().and_then(|()| read()))
    };
    // SAFETY: `key` was opened above and is closed once
    unsafe { RegCloseKey(key) };
    named.map(|bytes| bytes.to_vec())
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn named() -> Option<Vec<u8>> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn this_boot_is_named_the_same_each_time_it_is_read() {
        assert_eq!(this_boot(), this_boot());
        #[cfg(any(target_os = "linux", target_os = "macos", windows))]
        assert!(this_boot().same(&this_boot()), "the system names its boot");
    }

    /// The system's name for a boot is a UUID, or on Windows the 16 random bytes the first process wrote: never one
    /// or two bytes, which every boot could share
    #[cfg(any(target_os = "linux", target_os = "macos", windows))]
    #[test]
    fn a_boot_is_named_by_at_least_sixteen_bytes() {
        let name = named().expect("the system names its boot");
        assert!(name.len() >= 16, "{name:?}");
    }

    #[test]
    fn no_boot_is_another_boot_than_any_itself_included() {
        assert!(!Boot::none().same(&Boot::none()));
        assert!(!Boot::none().same(&Boot::of(b"a")));
        assert!(!Boot::of(b"a").same(&Boot::of(b"b")));
        assert!(Boot::of(b"a").same(&Boot::of(b"a")));
    }
}
