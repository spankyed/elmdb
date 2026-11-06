//! Waking other processes that hold this database open: a commit here tells each of them its sequence, so one holding
//! tracked reads learns right away that another process committed. What changed is in the change log, so a wake carries
//! nothing a reader depends on: one lost costs delay, and a stray packet costs a read of the log.
//!
//! A handle that wants waking binds a loopback UDP port and files it as `wake/<pid>-<port>` in the database's directory,
//! holding a lock on that file for as long as it listens, and every other handle's commits send to what's filed there. A
//! file nobody holds locked was left by a process that has gone, which a reader removes: the lock goes with its holder
//! however it ends, on every platform and across PID namespaces, where a process id says nothing. Loopback UDP is the one
//! mechanism that behaves alike on every platform here, with nothing to authenticate since a wake asks only for a read. A
//! handle that doesn't listen binds nothing, and a commit with nobody to tell costs a `stat` of the directory. Listening
//! writes into the database's directory, so a handle on a directory it can't write can't listen.

use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime};

use crate::error::{Error, ErrorCode, Result};
use crate::sync::PlainMutex;

/// What a listener is told: the latest sequence it was woken for
pub type WakeListener = Arc<dyn Fn(u64) + Send + Sync>;

#[derive(Default)]
struct Peers {
    /// The directory's modified time when `addresses` was read, once enough older than that read that a port filed since
    /// must have moved it: a commit reads the directory only when it has moved
    settled: Option<SystemTime>,
    addresses: Vec<SocketAddr>,
    /// What sends to them, bound the first time there's someone to send to
    socket: Option<UdpSocket>,
    /// This handle's own port, while it listens: it doesn't wake itself
    own_port: Option<u16>,
}

struct Listening {
    stopping: Arc<AtomicBool>,
    port: u16,
    /// The listening socket, cloned, which `stop` wakes the thread from: binding another could fail
    waker: UdpSocket,
    path: PathBuf,
    /// Held locked while listening, which is what says the port is live
    file: std::fs::File,
    thread: JoinHandle<()>,
}

pub(crate) struct Wake {
    directory: PathBuf,
    peers: PlainMutex<Peers>,
    listening: PlainMutex<Option<Listening>>,
}

const LOOPBACK: (Ipv4Addr, u16) = (Ipv4Addr::LOCALHOST, 0);

/// Coarser than any filesystem's directory times here: FAT's 2 seconds is the worst of them
const TIMESTAMP_GRAIN: Duration = Duration::from_secs(3);

/// Whether a filed port's listener has gone: nobody holds its file locked. One that can't be opened is left alone
fn listener_is_gone(path: &Path) -> bool {
    let Ok(file) = std::fs::File::open(path) else { return false };
    file.try_lock().is_ok()
}

/// A failure of a listening socket's `recv` that a later one can come back from: a datagram larger than a wake, which
/// Windows refuses rather than cutting short, or the reset a send to a closed port can leave
fn passing(error: &std::io::Error) -> bool {
    use std::io::ErrorKind;
    #[cfg(windows)]
    const MESSAGE_TOO_LONG: i32 = 10040;
    #[cfg(not(windows))]
    const MESSAGE_TOO_LONG: i32 = libc::EMSGSIZE;
    matches!(error.kind(), ErrorKind::Interrupted | ErrorKind::ConnectionReset | ErrorKind::ConnectionRefused)
        || error.raw_os_error() == Some(MESSAGE_TOO_LONG)
}

impl Wake {
    pub(crate) fn new(database: &Path) -> Self {
        Self { directory: database.join("wake"), peers: PlainMutex::default(), listening: PlainMutex::default() }
    }

    /// Tells every other listening handle of the commit with this `sequence`. Whatever goes wrong here costs a wake, and
    /// the commit has happened, so nothing goes back to the writer
    pub(crate) fn notify(&self, sequence: u64) {
        let Ok(modified) = std::fs::metadata(&self.directory).and_then(|found| found.modified()) else { return };
        let mut peers = self.peers.lock();
        if peers.settled != Some(modified) {
            let reading = SystemTime::now();
            peers.addresses = self.read_peers(peers.own_port);
            // A file's time is only as fine as its filesystem's clock, so a port filed just after this read can leave the
            // time as it was: a recent time isn't trusted yet
            let settled = reading.duration_since(modified).is_ok_and(|age| age > TIMESTAMP_GRAIN);
            peers.settled = settled.then_some(modified);
        }
        if peers.addresses.is_empty() {
            return;
        }
        if peers.socket.is_none() {
            peers.socket =
                UdpSocket::bind(LOOPBACK).and_then(|socket| socket.set_nonblocking(true).map(|()| socket)).ok();
        }
        let Some(socket) = &peers.socket else { return };
        for address in &peers.addresses {
            let _ = socket.send_to(&sequence.to_be_bytes(), address);
        }
    }

    /// The ports filed, without this handle's, removing those of processes that have gone
    fn read_peers(&self, own_port: Option<u16>) -> Vec<SocketAddr> {
        let Ok(entries) = std::fs::read_dir(&self.directory) else { return Vec::new() };
        entries
            .flatten()
            .filter_map(|entry| {
                let name = entry.file_name();
                let (_, port) = name.to_str()?.split_once('-')?;
                let port: u16 = port.parse().ok()?;
                if Some(port) == own_port {
                    return None;
                }
                if listener_is_gone(&entry.path()) {
                    let _ = std::fs::remove_file(entry.path());
                    return None;
                }
                Some(SocketAddr::from((Ipv4Addr::LOCALHOST, port)))
            })
            .collect()
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
        let socket = UdpSocket::bind(LOOPBACK)?;
        let port = socket.local_addr()?.port();
        let waker = socket.try_clone()?;
        std::fs::create_dir_all(&self.directory)?;
        let path = self.directory.join(format!("{}-{port}", std::process::id()));
        let file = std::fs::File::create(&path)?;
        file.lock()?;
        self.peers.lock().own_port = Some(port);
        let stopping = Arc::new(AtomicBool::new(false));
        let stop = Arc::clone(&stopping);
        let thread = std::thread::Builder::new().name("elmdb-wake".into()).spawn(move || {
            let mut bytes = [0u8; 8];
            loop {
                match socket.recv(&mut bytes) {
                    Ok(_) => {}
                    Err(error) if passing(&error) => continue,
                    // Nothing wakes this again, and what was missed is in the log for whoever reads it next
                    Err(_) => return,
                }
                if stop.load(Ordering::Acquire) {
                    return;
                }
                let mut latest = u64::from_be_bytes(bytes);
                // What else has arrived is the same news. A full queue dropping a wake loses nothing: something older is
                // still queued, so the listener runs after the dropped one's commit
                if socket.set_nonblocking(true).is_ok() {
                    while socket.recv(&mut bytes).is_ok() {
                        latest = latest.max(u64::from_be_bytes(bytes));
                    }
                    let _ = socket.set_nonblocking(false);
                }
                if stop.load(Ordering::Acquire) {
                    return;
                }
                listener(latest);
            }
        })?;
        *listening = Some(Listening { stopping, port, waker, path, file, thread });
        Ok(())
    }

    /// Stops listening, waiting for a listener that's running, unless this is it
    pub(crate) fn stop(&self) {
        let Some(Listening { stopping, port, waker, path, file, thread }) = self.listening.lock().take() else {
            return;
        };
        stopping.store(true, Ordering::Release);
        // Unlocked and closed first: Windows removes no file a handle holds open
        drop(file);
        let _ = std::fs::remove_file(path);
        self.peers.lock().own_port = None;
        let _ = waker.send_to(&[0; 8], (Ipv4Addr::LOCALHOST, port));
        if thread.thread().id() != std::thread::current().id() {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    fn heard(wake: &Wake) -> mpsc::Receiver<u64> {
        let (told, heard) = mpsc::channel();
        wake.listen(Arc::new(move |sequence| {
            let _ = told.send(sequence);
        }))
        .unwrap();
        heard
    }

    #[test]
    fn a_commit_with_nobody_listening_binds_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let wake = Wake::new(dir.path());
        wake.notify(1);
        std::fs::create_dir(dir.path().join("wake")).unwrap();
        wake.notify(2);
        assert!(wake.peers.lock().socket.is_none());
    }

    #[test]
    fn a_settled_directory_is_read_again_once_a_listener_files_its_port() {
        let dir = tempfile::tempdir().unwrap();
        let (writer, reader) = (Wake::new(dir.path()), Wake::new(dir.path()));
        std::fs::create_dir(dir.path().join("wake")).unwrap();
        let long_ago = SystemTime::now() - Duration::from_secs(60);
        std::fs::File::open(dir.path().join("wake")).unwrap().set_modified(long_ago).unwrap();
        writer.notify(1);
        assert_eq!(
            writer.peers.lock().settled,
            Some(long_ago),
            "an old time is trusted, so the next commit only stats"
        );

        let heard = heard(&reader);
        writer.notify(2);
        assert_eq!(heard.recv_timeout(Duration::from_secs(5)).unwrap(), 2);
        assert_eq!(writer.peers.lock().settled, None, "a time this recent isn't trusted yet");
        reader.stop();
    }

    #[test]
    fn a_port_whose_listener_has_gone_is_removed_and_a_live_one_kept() {
        let dir = tempfile::tempdir().unwrap();
        let (writer, reader) = (Wake::new(dir.path()), Wake::new(dir.path()));
        let heard = heard(&reader);
        let left = dir.path().join("wake").join("1-9");
        std::fs::File::create(&left).unwrap();
        writer.notify(3);
        assert!(!left.exists(), "nobody held it");
        assert_eq!(heard.recv_timeout(Duration::from_secs(5)).unwrap(), 3, "the live one was told");
        assert_eq!(std::fs::read_dir(dir.path().join("wake")).unwrap().count(), 1);
        reader.stop();
        assert_eq!(std::fs::read_dir(dir.path().join("wake")).unwrap().count(), 0, "a stopped listener files nothing");
    }

    #[test]
    fn a_datagram_larger_than_a_wake_does_not_end_the_listener() {
        let dir = tempfile::tempdir().unwrap();
        let reader = Wake::new(dir.path());
        let heard = heard(&reader);
        let port = reader.peers.lock().own_port.unwrap();
        let stray = UdpSocket::bind(LOOPBACK).unwrap();
        stray.send_to(&[7; 64], (Ipv4Addr::LOCALHOST, port)).unwrap();
        let _ = heard.recv_timeout(Duration::from_secs(5));
        stray.send_to(&9u64.to_be_bytes(), (Ipv4Addr::LOCALHOST, port)).unwrap();
        let mut last = 0;
        while let Ok(sequence) = heard.recv_timeout(Duration::from_secs(5)) {
            last = sequence;
            if last == 9 {
                break;
            }
        }
        assert_eq!(last, 9, "still listening");
        reader.stop();
    }
}
