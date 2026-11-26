//! Pure-Rust pre-map header validator and active meta-page reader for LMDB `data.mdb`.
//!
//! Reading `data.mdb` via regular file I/O before `mmap` defends against `SIGBUS` on
//! truncated/torn files and provides the cryptographic `roots_sha256` consumed by
//! `elmdb_storage::DurabilityGate`.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use elmdb_storage::StorageError;
use sha2::{Digest, Sha256};

const MDB_MAGIC: u32 = 0xBEEF_C0DE;
const MDB_DATA_VERSION: u32 = 1;
const P_META: u16 = 0x08;

// 64-bit LMDB 0.9.x/1.0 `MDB_page` + `MDB_meta` offsets (verified by unit tests against live LMDB).
const PAGE_HDR_SIZE: usize = 16;
const META_STRUCT_SIZE: usize = 136;
const MIN_META_BYTES: usize = PAGE_HDR_SIZE + META_STRUCT_SIZE;

/// Decoded LMDB meta page header fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MetaPageInfo {
    pub slot: u8,
    pub page_size: u32,
    pub map_size: u64,
    pub free_root: u64,
    pub main_root: u64,
    pub last_pg: u64,
    pub txnid: u64,
    pub roots_sha256: [u8; 32],
}

fn read_u16_ne(buf: &[u8], off: usize) -> u16 {
    u16::from_ne_bytes(buf[off..off + 2].try_into().expect("2 bytes"))
}

fn read_u32_ne(buf: &[u8], off: usize) -> u32 {
    u32::from_ne_bytes(buf[off..off + 4].try_into().expect("4 bytes"))
}

fn read_u64_ne(buf: &[u8], off: usize) -> u64 {
    u64::from_ne_bytes(buf[off..off + 8].try_into().expect("8 bytes"))
}

fn parse_meta_page(slot: u8, buf: &[u8]) -> Option<MetaPageInfo> {
    if buf.len() < MIN_META_BYTES {
        return None;
    }
    let pgno = read_u64_ne(buf, 0);
    let flags = read_u16_ne(buf, 10);
    if pgno != u64::from(slot) || (flags & P_META) == 0 {
        return None;
    }

    let m = &buf[PAGE_HDR_SIZE..MIN_META_BYTES];
    let magic = read_u32_ne(m, 0);
    let version = read_u32_ne(m, 4);
    if magic != MDB_MAGIC || version != MDB_DATA_VERSION {
        return None;
    }

    let map_size = read_u64_ne(m, 16);
    // `mm_dbs[0]` (FREE_DBI) starts at offset 24 (48 bytes); `md_pad` holds `mm_psize`.
    let page_size = read_u32_ne(m, 24);
    if !page_size.is_power_of_two() || !(4096..=65536).contains(&page_size) {
        return None;
    }
    let free_root = read_u64_ne(m, 24 + 40);
    // `mm_dbs[1]` (MAIN_DBI) starts at offset 72 (48 bytes).
    let main_root = read_u64_ne(m, 72 + 40);
    let last_pg = read_u64_ne(m, 120);
    let txnid = read_u64_ne(m, 128);

    // Hash the exact `mm_dbs[0..2]`, `mm_last_pg`, and `mm_txnid` descriptors.
    let mut hasher = Sha256::new();
    hasher.update(&m[24..136]);
    let roots_sha256: [u8; 32] = hasher.finalize().into();

    Some(MetaPageInfo { slot, page_size, map_size, free_root, main_root, last_pg, txnid, roots_sha256 })
}

/// Reads page 0 and page 1 from `data.mdb` using standard seek/read I/O (never mmap)
/// and returns the valid meta page with the higher `txnid`.
pub fn read_active_meta_page(data_mdb: &Path) -> Result<MetaPageInfo, StorageError> {
    let mut file = File::open(data_mdb)?;
    let len = file.metadata()?.len();
    if len < MIN_META_BYTES as u64 {
        return Err(StorageError::Corrupted(format!("data.mdb too small for meta page ({len} bytes)")));
    }

    let mut page0 = [0u8; MIN_META_BYTES];
    file.seek(SeekFrom::Start(0))?;
    file.read_exact(&mut page0)?;
    let m0 = parse_meta_page(0, &page0);

    let page_size = m0.map_or(4096u64, |m| u64::from(m.page_size));
    let m1 = if len >= page_size + (MIN_META_BYTES as u64) {
        let mut page1 = [0u8; MIN_META_BYTES];
        file.seek(SeekFrom::Start(page_size))?;
        if file.read_exact(&mut page1).is_ok() { parse_meta_page(1, &page1) } else { None }
    } else {
        None
    };

    match (m0, m1) {
        (Some(a), Some(b)) => Ok(if b.txnid > a.txnid { b } else { a }),
        (Some(a), None) => Ok(a),
        (None, Some(b)) => Ok(b),
        (None, None) => Err(StorageError::Corrupted("neither LMDB meta page 0 nor page 1 is valid".into())),
    }
}

/// Pre-map sanity check on an existing `data.mdb`: verifies at least one valid meta page
/// exists and that `main_root` / `free_root` (if non-empty) lie strictly inside the file length
/// so opening the mmap will not trigger `SIGBUS`.
pub fn verify_data_file_headers(data_mdb: &Path) -> Result<Option<MetaPageInfo>, StorageError> {
    if !data_mdb.exists() {
        return Ok(None);
    }
    let len = std::fs::metadata(data_mdb)?.len();
    if len == 0 {
        return Ok(None);
    }
    let meta = read_active_meta_page(data_mdb)?;
    let max_pgno = len / u64::from(meta.page_size);
    for (label, root) in [("main_root", meta.main_root), ("free_root", meta.free_root)] {
        if root != u64::MAX && root >= max_pgno {
            return Err(StorageError::Corrupted(format!(
                "LMDB {label} page {root} lies past data.mdb EOF ({max_pgno} pages)"
            )));
        }
    }
    Ok(Some(meta))
}
