//! The boot a record was written in: an identity the machine loses when it stops and no clock makes
//! (`docs/roadmap/synced-commit.md`, The design). A record of another boot than this one was written before the machine
//! last stopped, so what was written after it and not synced may be gone.
//!
//! Linux names each boot in `/proc/sys/kernel/random/boot_id`, and Apple's systems in `kern.bootsessionuuid`. Where none
//! can be had, on Windows until its own is built, the boot is none, which is another boot than any: a process's death
//! there costs the commits after its last sync, as a loss would, and opens no torn file.

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

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn named() -> Option<Vec<u8>> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn this_boot_is_named_the_same_each_time_it_is_read() {
        assert_eq!(this_boot(), this_boot());
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        assert!(this_boot().same(&this_boot()), "the system names its boot");
    }

    #[test]
    fn no_boot_is_another_boot_than_any_itself_included() {
        assert!(!Boot::none().same(&Boot::none()));
        assert!(!Boot::none().same(&Boot::of(b"a")));
        assert!(!Boot::of(b"a").same(&Boot::of(b"b")));
        assert!(Boot::of(b"a").same(&Boot::of(b"a")));
    }
}
