//! Waking other processes that hold this database open: a commit here rings each of them, so one holding tracked reads
//! learns right away that another process committed. Whether one did is the store's to answer (`committed_elsewhere`),
//! so a wake carries nothing: one lost costs delay, and a stray one costs an ask.
//!
//! A handle that wants waking files a mailbox in `wake/` in the database's directory, and every other handle's commits
//! ring what's filed there. A mailbox is the system's own wake-up primitive, whose end goes with its owner however that
//! ends, so a mailbox whose owner has gone can't be rung, and the committer that finds so removes it:
//!
//! - **Unix:** a FIFO, as Realm wakes processes sharing a database file. Its owner holds the read end open, and a
//!   committer writes a byte with the write end opened without blocking, which fails (`ENXIO`) when nobody reads it. It
//!   is made under a name readers pass over and renamed into place once its read end is open, so no reader finds it
//!   unowned. A FIFO's path, unlike a socket's, has no length limit.
//! - **Windows:** a named auto-reset event, the file in `wake/` naming it. An event that won't open has no handle left
//!   to it, so its owner has gone.
//!
//! Nothing binds a socket, so a sandbox that refuses one can still listen. A handle that doesn't listen files nothing,
//! and a commit with nobody to tell costs a `stat` of the directory. Listening writes into the database's directory, so a
//! handle on a directory it can't write, or on a filesystem with no FIFOs, can't listen.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime};

use crate::error::{Error, ErrorCode, Result};
use crate::sync::PlainMutex;

/// What a listener is told: something committed. Its caller asks what
pub type WakeListener = Arc<dyn Fn() + Send + Sync>;

#[derive(Default)]
struct Peers {
    /// The directory's modified time when `mailboxes` was read, once enough older than that read that a mailbox filed since
    /// must have moved it: a commit reads the directory only when it has moved
    settled: Option<SystemTime>,
    mailboxes: Vec<PathBuf>,
    /// This handle's own mailbox, while it listens: it doesn't wake itself
    own: Option<PathBuf>,
}

struct Listening {
    stopping: Arc<AtomicBool>,
    mailbox: Arc<mailbox::Mailbox>,
    thread: JoinHandle<()>,
}

pub(crate) struct Wake {
    directory: PathBuf,
    peers: PlainMutex<Peers>,
    listening: PlainMutex<Option<Listening>>,
}

/// Coarser than any filesystem's directory times here: FAT's 2 seconds is the worst of them
const TIMESTAMP_GRAIN: Duration = Duration::from_secs(3);

/// Far longer than making a mailbox takes, from its FIFO to its filing: one still half made after this was left
const HALF_MADE: Duration = Duration::from_secs(60);

/// What ringing a mailbox found
#[derive(Debug, PartialEq)]
enum Rung {
    Told,
    /// Nobody owns it: its owner has gone
    Gone,
    /// Neither could be said, as for a mailbox removed meanwhile
    Unknown,
}

impl Wake {
    pub(crate) fn new(database: &Path) -> Self {
        Self { directory: database.join("wake"), peers: PlainMutex::default(), listening: PlainMutex::default() }
    }

    /// Rings every other listening handle. Whatever goes wrong here costs a wake, and the commit has happened, so nothing
    /// goes back to the writer
    pub(crate) fn notify(&self) {
        let Ok(modified) = std::fs::metadata(&self.directory).and_then(|found| found.modified()) else { return };
        let mut peers = self.peers.lock();
        if peers.settled != Some(modified) {
            let reading = SystemTime::now();
            peers.mailboxes = self.read_mailboxes(peers.own.as_deref());
            // A file's time is only as fine as its filesystem's clock, so a mailbox filed just after this read can leave
            // the time as it was: a recent time isn't trusted yet
            let settled = reading.duration_since(modified).is_ok_and(|age| age > TIMESTAMP_GRAIN);
            peers.settled = settled.then_some(modified);
        }
        peers.mailboxes.retain(|path| match mailbox::ring(path) {
            Rung::Gone => {
                let _ = std::fs::remove_file(path);
                false
            }
            Rung::Told | Rung::Unknown => true,
        });
    }

    /// The mailboxes filed, without this handle's and those being made. One left half made, by a listener that ended
    /// between making it and filing it, is removed once it is older than any making takes and nobody owns it
    fn read_mailboxes(&self, own: Option<&Path>) -> Vec<PathBuf> {
        let Ok(entries) = std::fs::read_dir(&self.directory) else { return Vec::new() };
        let mut mailboxes = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if !entry.file_name().to_string_lossy().starts_with('.') {
                if Some(path.as_path()) != own {
                    mailboxes.push(path);
                }
                continue;
            }
            let old = entry
                .metadata()
                .and_then(|found| found.modified())
                .ok()
                .and_then(|modified| SystemTime::now().duration_since(modified).ok())
                .is_some_and(|age| age > HALF_MADE);
            if old && mailbox::ring(&path) == Rung::Gone {
                let _ = std::fs::remove_file(&path);
            }
        }
        mailboxes
    }

    /// Tells this handle's own listener, as a wake from another process would: on its thread, so whoever calls this holds
    /// nothing the listener may need
    pub(crate) fn wake_self(&self) {
        if let Some(listening) = self.listening.lock().as_ref() {
            listening.mailbox.ring_own();
        }
    }

    /// Starts telling `listener` of commits other processes make. There is one listener: a second is refused. It runs on a
    /// thread of its own, and is told once for a burst
    pub(crate) fn listen(&self, listener: WakeListener) -> Result<()> {
        let mut listening = self.listening.lock();
        if listening.is_some() {
            return Err(Error::new(
                ErrorCode::AlreadyListening,
                "Something is already hearing commits from other processes",
            ));
        }
        std::fs::create_dir_all(&self.directory)?;
        let mailbox = Arc::new(mailbox::Mailbox::file(&self.directory)?);
        self.peers.lock().own = Some(mailbox.path().to_path_buf());
        let stopping = Arc::new(AtomicBool::new(false));
        let (stop, waiting) = (Arc::clone(&stopping), Arc::clone(&mailbox));
        let thread = std::thread::Builder::new().name("elmdb-wake".into()).spawn(move || {
            loop {
                // Nothing wakes this again after a failure, and whoever asks next learns what was missed
                if waiting.wait().is_err() || stop.load(Ordering::Acquire) {
                    return;
                }
                listener();
            }
        })?;
        *listening = Some(Listening { stopping, mailbox, thread });
        Ok(())
    }

    /// Stops listening, waiting for a listener that's running, unless this is it. Its mailbox goes once nothing holds it
    pub(crate) fn stop(&self) {
        let Some(Listening { stopping, mailbox, thread }) = self.listening.lock().take() else { return };
        stopping.store(true, Ordering::Release);
        self.peers.lock().own = None;
        mailbox.ring_own();
        if thread.thread().id() != std::thread::current().id() {
            let _ = thread.join();
        }
    }
}

/// A name no other mailbox has: this process's id, which says nothing on its own across PID namespaces or once reused,
/// and enough randomness that it needn't
fn mailbox_name() -> String {
    let random = getrandom::u64().expect("the OS supplies random bytes");
    format!("{}-{random:016x}", std::process::id())
}

#[cfg(unix)]
mod mailbox {
    use std::ffi::CString;
    use std::fs::File;
    use std::io::{Read, Write};
    use std::os::fd::AsRawFd;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::{FileTypeExt, OpenOptionsExt};
    use std::path::{Path, PathBuf};

    use super::Rung;

    pub(super) struct Mailbox {
        path: PathBuf,
        read: File,
        /// This handle's own write end, which keeps the FIFO from reading as ended between wakes, and rings it
        write: File,
    }

    pub(super) fn open_write(path: &Path) -> std::io::Result<File> {
        std::fs::OpenOptions::new().write(true).custom_flags(libc::O_NONBLOCK).open(path)
    }

    /// Writes a wake to a FIFO whose reader may have gone since it was opened, without the `SIGPIPE` that would end a
    /// process not ignoring it: elmdb is a library, and its host decides what that signal does. A full FIFO already
    /// holds a wake, so `EAGAIN` is as good as written
    pub(super) fn write_wake(file: &File) -> std::io::Result<()> {
        #[cfg(target_os = "macos")]
        {
            // SAFETY: a descriptor this function's caller holds open; the flag only says what a broken pipe does
            unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETNOSIGPIPE, 1) };
            written((&*file).write(&[1]))
        }
        #[cfg(not(target_os = "macos"))]
        // SAFETY: signal sets made here and the calling thread's own mask, restored before returning
        unsafe {
            let mut pipe: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&raw mut pipe);
            libc::sigaddset(&raw mut pipe, libc::SIGPIPE);
            let mut pending: libc::sigset_t = std::mem::zeroed();
            libc::sigpending(&raw mut pending);
            let already = libc::sigismember(&raw const pending, libc::SIGPIPE) == 1;
            let mut kept: libc::sigset_t = std::mem::zeroed();
            libc::pthread_sigmask(libc::SIG_BLOCK, &raw const pipe, &raw mut kept);
            let result = (&*file).write(&[1]);
            if !already && matches!(&result, Err(error) if error.raw_os_error() == Some(libc::EPIPE)) {
                // The signal the write raised, taken while blocked so it never reaches the thread
                let now = libc::timespec { tv_sec: 0, tv_nsec: 0 };
                libc::sigtimedwait(&raw const pipe, std::ptr::null_mut(), &raw const now);
            }
            libc::pthread_sigmask(libc::SIG_SETMASK, &raw const kept, std::ptr::null_mut());
            written(result)
        }
    }

    fn written(result: std::io::Result<usize>) -> std::io::Result<()> {
        match result {
            Ok(_) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => Ok(()),
            Err(error) => Err(error),
        }
    }

    impl Mailbox {
        pub(super) fn file(directory: &Path) -> std::io::Result<Self> {
            let name = super::mailbox_name();
            let making = directory.join(format!(".{name}"));
            let c_path = CString::new(making.as_os_str().as_bytes()).expect("a path holds no NUL");
            // SAFETY: a valid C string; mkfifo creates nothing else
            if unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) } != 0 {
                return Err(std::io::Error::last_os_error());
            }
            let opened = (|| {
                let read = std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NONBLOCK).open(&making)?;
                let write = open_write(&making)?;
                let path = directory.join(&name);
                std::fs::rename(&making, &path)?;
                Ok(Self { path, read, write })
            })();
            if opened.is_err() {
                let _ = std::fs::remove_file(&making);
            }
            opened
        }

        pub(super) fn path(&self) -> &Path {
            &self.path
        }

        /// Blocks until rung, then takes every ring waiting: one wake for a burst
        pub(super) fn wait(&self) -> std::io::Result<()> {
            let mut poll = libc::pollfd { fd: self.read.as_raw_fd(), events: libc::POLLIN, revents: 0 };
            loop {
                // SAFETY: one valid pollfd, for a descriptor this mailbox holds open
                if unsafe { libc::poll(&raw mut poll, 1, -1) } >= 0 {
                    break;
                }
                let error = std::io::Error::last_os_error();
                if error.kind() != std::io::ErrorKind::Interrupted {
                    return Err(error);
                }
            }
            let mut bytes = [0u8; 64];
            loop {
                match (&self.read).read(&mut bytes) {
                    Ok(0) => return Ok(()),
                    Ok(_) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => return Ok(()),
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(error) => return Err(error),
                }
            }
        }

        pub(super) fn ring_own(&self) {
            // A full FIFO already holds a wake
            let _ = (&self.write).write(&[1]);
        }
    }

    impl Drop for Mailbox {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }

    /// Rings the mailbox at `path`. Anything there that isn't a FIFO isn't a mailbox, and is gone too
    pub(super) fn ring(path: &Path) -> Rung {
        match std::fs::symlink_metadata(path) {
            Ok(found) if !found.file_type().is_fifo() => return Rung::Gone,
            Ok(_) => {}
            Err(_) => return Rung::Unknown,
        }
        match open_write(path) {
            Ok(file) => match write_wake(&file) {
                Ok(()) => Rung::Told,
                // Its reader went between the open and the write
                Err(error) if error.raw_os_error() == Some(libc::EPIPE) => Rung::Gone,
                Err(_) => Rung::Unknown,
            },
            Err(error) if error.raw_os_error() == Some(libc::ENXIO) => Rung::Gone,
            Err(_) => Rung::Unknown,
        }
    }
}

#[cfg(windows)]
mod mailbox {
    use std::os::windows::ffi::OsStrExt;
    use std::path::{Path, PathBuf};

    use windows_sys::Win32::Foundation::{CloseHandle, ERROR_FILE_NOT_FOUND, GetLastError, HANDLE, WAIT_OBJECT_0};
    use windows_sys::Win32::System::Threading::{
        CreateEventW, EVENT_MODIFY_STATE, INFINITE, OpenEventW, SetEvent, WaitForSingleObject,
    };

    use super::Rung;

    pub(super) struct Mailbox {
        path: PathBuf,
        event: HANDLE,
    }

    // SAFETY: an event's handle is the kernel's, and waiting on and setting it are safe from any thread
    unsafe impl Send for Mailbox {}
    // SAFETY: as for Send
    unsafe impl Sync for Mailbox {}

    /// The event a mailbox's file names, in this session's namespace
    fn event_name(path: &Path) -> Vec<u16> {
        let name = path.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
        std::ffi::OsStr::new(&format!("Local\\elmdb-wake-{name}")).encode_wide().chain([0]).collect()
    }

    impl Mailbox {
        pub(super) fn file(directory: &Path) -> std::io::Result<Self> {
            let path = directory.join(super::mailbox_name());
            let name = event_name(&path);
            // SAFETY: no security attributes, an auto-reset event that starts unset, and a NUL-terminated name
            let event = unsafe { CreateEventW(std::ptr::null(), 0, 0, name.as_ptr()) };
            if event.is_null() {
                return Err(std::io::Error::last_os_error());
            }
            let mailbox = Self { path, event };
            // Filed once the event exists, so no reader finds it unowned
            std::fs::File::create(&mailbox.path)?;
            Ok(mailbox)
        }

        pub(super) fn path(&self) -> &Path {
            &self.path
        }

        /// Blocks until rung; an auto-reset event takes every ring waiting as one
        pub(super) fn wait(&self) -> std::io::Result<()> {
            // SAFETY: a handle this mailbox holds open
            if unsafe { WaitForSingleObject(self.event, INFINITE) } == WAIT_OBJECT_0 {
                Ok(())
            } else {
                Err(std::io::Error::last_os_error())
            }
        }

        pub(super) fn ring_own(&self) {
            // SAFETY: as above
            unsafe { SetEvent(self.event) };
        }
    }

    impl Drop for Mailbox {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
            // SAFETY: opened in `file`, closed once, here
            unsafe { CloseHandle(self.event) };
        }
    }

    /// Rings the mailbox at `path`, opening its event for as long as that takes: a handle kept would keep the event alive
    /// after its owner had gone
    pub(super) fn ring(path: &Path) -> Rung {
        let name = event_name(path);
        // SAFETY: a NUL-terminated name, and no handle inherited
        let event = unsafe { OpenEventW(EVENT_MODIFY_STATE, 0, name.as_ptr()) };
        if event.is_null() {
            // SAFETY: read straight after the call that failed
            return if unsafe { GetLastError() } == ERROR_FILE_NOT_FOUND { Rung::Gone } else { Rung::Unknown };
        }
        // SAFETY: the handle just opened, closed once, here
        unsafe {
            SetEvent(event);
            CloseHandle(event);
        }
        Rung::Told
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::os::unix::fs::OpenOptionsExt;
    use std::sync::mpsc;

    /// A directory opened so its times can be set: Windows opens one only with backup semantics, and to write its times
    fn directory(path: &Path) -> std::fs::File {
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            const FILE_WRITE_ATTRIBUTES: u32 = 0x100;
            const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
            std::fs::OpenOptions::new()
                .access_mode(FILE_WRITE_ATTRIBUTES)
                .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
                .open(path)
                .unwrap()
        }
        #[cfg(not(windows))]
        std::fs::File::open(path).unwrap()
    }

    fn heard(wake: &Wake) -> mpsc::Receiver<()> {
        let (told, heard) = mpsc::channel();
        wake.listen(Arc::new(move || {
            let _ = told.send(());
        }))
        .unwrap();
        heard
    }

    const SETTLES: Duration = Duration::from_secs(10);

    #[test]
    fn a_commit_with_nobody_listening_files_and_rings_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let wake = Wake::new(dir.path());
        wake.notify();
        assert!(!dir.path().join("wake").exists(), "nothing filed");
        std::fs::create_dir(dir.path().join("wake")).unwrap();
        wake.notify();
        assert!(wake.peers.lock().mailboxes.is_empty());
    }

    #[test]
    fn a_settled_directory_is_read_again_once_a_listener_files_its_mailbox() {
        let dir = tempfile::tempdir().unwrap();
        let (writer, reader) = (Wake::new(dir.path()), Wake::new(dir.path()));
        std::fs::create_dir(dir.path().join("wake")).unwrap();
        let long_ago = SystemTime::now() - Duration::from_secs(60);
        directory(&dir.path().join("wake")).set_modified(long_ago).unwrap();
        writer.notify();
        assert_eq!(
            writer.peers.lock().settled,
            Some(long_ago),
            "an old time is trusted, so the next commit only stats"
        );

        let heard = heard(&reader);
        writer.notify();
        heard.recv_timeout(SETTLES).expect("rung");
        assert_eq!(writer.peers.lock().settled, None, "a time this recent isn't trusted yet");
        reader.stop();
    }

    /// A mailbox as a killed listener leaves it: filed, with nobody owning it
    fn file_ownerless(directory: &Path) -> PathBuf {
        std::fs::create_dir_all(directory).unwrap();
        let path = directory.join(super::mailbox_name());
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            let c_path = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
            // SAFETY: a valid C string
            assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);
        }
        #[cfg(windows)]
        std::fs::File::create(&path).unwrap();
        path
    }

    #[test]
    fn a_mailbox_whose_listener_has_gone_is_removed_and_a_live_one_rung() {
        let dir = tempfile::tempdir().unwrap();
        let (writer, reader) = (Wake::new(dir.path()), Wake::new(dir.path()));
        let heard = heard(&reader);
        let left = file_ownerless(&dir.path().join("wake"));
        let stray = dir.path().join("wake").join("1-9");
        std::fs::write(&stray, b"").unwrap();
        writer.notify();
        assert!(!left.exists(), "nobody owned it");
        #[cfg(unix)]
        assert!(!stray.exists(), "a file that isn't a FIFO isn't a mailbox");
        let _ = std::fs::remove_file(&stray);
        heard.recv_timeout(SETTLES).expect("the live one was rung");
        assert_eq!(std::fs::read_dir(dir.path().join("wake")).unwrap().count(), 1);
        reader.stop();
        assert_eq!(std::fs::read_dir(dir.path().join("wake")).unwrap().count(), 0, "a stopped listener files nothing");
    }

    /// Leaves `SIGPIPE` to end the process, as a host that doesn't ignore it does, and writes a wake to a FIFO whose
    /// reader has gone since it was opened
    #[cfg(unix)]
    #[test]
    #[ignore = "a helper a_wake_to_a_reader_gone_since_the_open_ends_no_process runs as a process of its own"]
    fn child_writes_to_a_fifo_whose_reader_has_gone() {
        let Ok(dir) = std::env::var("ELMDB_PIPE_DIR") else { return };
        // SAFETY: the default action, in a process of its own
        unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };
        let path = file_ownerless(Path::new(&dir));
        let reader = std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NONBLOCK).open(&path).unwrap();
        let writer = mailbox::open_write(&path).unwrap();
        drop(reader);
        let error = mailbox::write_wake(&writer).unwrap_err();
        assert_eq!(error.raw_os_error(), Some(libc::EPIPE));
        println!("survived");
    }

    #[cfg(unix)]
    #[test]
    fn a_wake_to_a_reader_gone_since_the_open_ends_no_process() {
        let dir = tempfile::tempdir().unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--ignored",
                "--exact",
                "store::wake::tests::child_writes_to_a_fifo_whose_reader_has_gone",
                "--nocapture",
            ])
            .env("ELMDB_PIPE_DIR", dir.path())
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(output.status.success() && stdout.contains("survived"), "{:?}: {stdout}", output.status);
    }

    #[cfg(unix)]
    #[test]
    fn a_mailbox_left_half_made_is_removed_once_it_is_old_and_one_being_made_is_left() {
        let dir = tempfile::tempdir().unwrap();
        let wake = dir.path().join("wake");
        let made = |name: &str| {
            let path = file_ownerless(&wake);
            let renamed = wake.join(name);
            std::fs::rename(&path, &renamed).unwrap();
            renamed
        };
        let left = made(".left");
        let long_ago = SystemTime::now() - Duration::from_secs(600);
        let times = std::fs::FileTimes::new().set_modified(long_ago);
        // A FIFO's times are set through a handle that doesn't wait for a reader
        std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(&left)
            .unwrap()
            .set_times(times)
            .unwrap();
        let making = made(".making");
        Wake::new(dir.path()).notify();
        assert!(!left.exists(), "left half made long ago, by nobody");
        assert!(making.exists(), "still being made, as far as anyone can tell");
    }

    #[test]
    fn rings_coming_together_wake_the_listener_and_it_goes_on_listening() {
        let dir = tempfile::tempdir().unwrap();
        let (writer, reader) = (Wake::new(dir.path()), Wake::new(dir.path()));
        let heard = heard(&reader);
        for _ in 0..1_000 {
            writer.notify();
        }
        heard.recv_timeout(SETTLES).expect("woken");
        while heard.try_recv().is_ok() {}
        writer.notify();
        heard.recv_timeout(SETTLES).expect("still listening");
        reader.stop();
    }

    #[test]
    fn a_handle_wakes_its_own_listener_and_no_other_commit_of_its_own_does() {
        let dir = tempfile::tempdir().unwrap();
        let wake = Wake::new(dir.path());
        let heard = heard(&wake);
        wake.notify();
        wake.wake_self();
        heard.recv_timeout(SETTLES).expect("woken by itself");
        assert!(wake.peers.lock().mailboxes.is_empty(), "its own mailbox isn't another's");
        wake.stop();
    }
}
