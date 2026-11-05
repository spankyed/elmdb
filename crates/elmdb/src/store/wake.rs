//! Waking other processes that hold this database open: a commit here tells each of them its sequence, so one holding
//! tracked reads learns right away that another process committed. What changed is in the change log, so a wake carries
//! nothing a reader depends on: one lost costs delay, and a stray packet costs a read of the log.
//!
//! A handle that wants waking binds a loopback UDP port and files it as `wake/<pid>-<port>` in the database's directory,
//! and every other handle's commits send to what's filed there. Loopback UDP is the one mechanism that behaves alike on
//! every platform here, with nothing to authenticate since a wake asks only for a read. A handle that doesn't listen binds
//! nothing, and a commit with nobody to tell costs a `stat` of the directory.

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
    file: PathBuf,
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

#[cfg(unix)]
fn process_is_gone(pid: i32) -> bool {
    // SAFETY: signal 0 delivers nothing and only asks whether the process exists
    let answer = unsafe { libc::kill(pid, 0) };
    answer != 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
}

/// No cheap way to ask without a dependency for it: a file a crash left costs a packet nobody reads
#[cfg(not(unix))]
fn process_is_gone(_pid: i32) -> bool {
    false
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
                let (pid, port) = name.to_str()?.split_once('-')?;
                let (pid, port): (i32, u16) = (pid.parse().ok()?, port.parse().ok()?);
                if process_is_gone(pid) {
                    let _ = std::fs::remove_file(entry.path());
                    return None;
                }
                (Some(port) != own_port).then(|| SocketAddr::from((Ipv4Addr::LOCALHOST, port)))
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
        std::fs::create_dir_all(&self.directory)?;
        let file = self.directory.join(format!("{}-{port}", std::process::id()));
        std::fs::File::create(&file)?;
        self.peers.lock().own_port = Some(port);
        let stopping = Arc::new(AtomicBool::new(false));
        let stop = Arc::clone(&stopping);
        let thread = std::thread::Builder::new().name("elmdb-wake".into()).spawn(move || {
            let mut bytes = [0u8; 8];
            loop {
                match socket.recv(&mut bytes) {
                    Ok(_) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
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
        *listening = Some(Listening { stopping, port, file, thread });
        Ok(())
    }

    /// Stops listening, waiting for a listener that's running, unless this is it
    pub(crate) fn stop(&self) {
        let Some(Listening { stopping, port, file, thread }) = self.listening.lock().take() else { return };
        stopping.store(true, Ordering::Release);
        let _ = std::fs::remove_file(file);
        self.peers.lock().own_port = None;
        if let Ok(socket) = UdpSocket::bind(LOOPBACK) {
            let _ = socket.send_to(&[0; 8], (Ipv4Addr::LOCALHOST, port));
        }
        if thread.thread().id() != std::thread::current().id() {
            let _ = thread.join();
        }
    }
}
