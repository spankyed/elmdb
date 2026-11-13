//! The pages a database reaches from the meta page of one commit, read from the file by seeks and never mapped.
//!
//! A file shorter than its meta page's pages is healthy when none of the pages the trees reach is past its end: LMDB
//! may leave the last free pages out of the file, and no read touches them. It is torn when one the trees reach is past
//! the end, or fails the walk's checks of a page, and then a read of it faults or reads what no commit wrote. The walk
//! tells the two apart before the store maps or reads anything past the end.
//!
//! The walk reads the pages of one commit while other processes may be committing, so it reads them under a read
//! transaction at that commit: LMDB gives none of the pages a snapshot reaches to a later commit while a reader holds
//! it. A `Meta` borrows the transaction it was read for, and `reach` takes a `Meta`, so the store's walk never
//! outlasts its snapshot.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::marker::PhantomData;
use std::path::Path;

use heed::{RoTxn, WithoutTls};

use crate::error::{Error, ErrorCode, Result};

/// The page header: the page number, two bytes of padding, the flags, and the bounds of the page's free space
const PAGE_HEADER: usize = 16;
const MAGIC: u32 = 0xBEEF_C0DE;
const P_BRANCH: u16 = 0x01;
const P_LEAF: u16 = 0x02;
const P_OVERFLOW: u16 = 0x04;
const F_BIGDATA: u16 = 0x01;
const F_SUBDATA: u16 = 0x02;
const F_DUPDATA: u16 = 0x04;
/// `P_INVALID`: the root of an empty tree
const NONE: u64 = u64::MAX;
/// The sub-database record a sub-database node holds, and where its root is in it
const DB_RECORD: usize = 48;
const DB_ROOT: usize = 40;
/// The meta page fields the walk reads, from the start of the page: the magic, the roots of the free and main trees, the
/// last page the commit names, and the transaction that committed it
const META_MAGIC: usize = PAGE_HEADER;
const META_FREE_ROOT: usize = PAGE_HEADER + 24 + DB_ROOT;
const META_MAIN_ROOT: usize = PAGE_HEADER + 24 + DB_RECORD + DB_ROOT;
const META_LAST_PAGE: usize = PAGE_HEADER + 24 + 2 * DB_RECORD;
const META_TXNID: usize = META_LAST_PAGE + 8;
const META_LEN: usize = META_TXNID + 8;

/// What a commit's meta page names: the roots of its free and main trees, and the last page of the file as it left
/// it. Held no longer than the read transaction at that commit, which keeps the trees' pages as the commit left them
pub(super) struct Meta<'snapshot> {
    free: u64,
    main: u64,
    pub(super) last_page: u64,
    _held: PhantomData<&'snapshot ()>,
}

/// What the walk found: the first page the trees reach past the end of the file, if any, and the highest page they reach
/// when none is past it
pub(super) struct Reach {
    pub(super) missing: Option<u64>,
    pub(super) highest: u64,
}

/// Walks the free and main trees a meta page names, in a file of `pages` whole pages of `psize` bytes. A page the trees
/// reach that is past the end, or does not carry its own number and a tree page's flags, is a torn file: the first is
/// named as missing, the second is refused as corrupt
pub(super) fn reach(file: &mut File, psize: usize, pages: u64, meta: &Meta<'_>, path: &Path) -> Result<Reach> {
    let mut walk = Walk { file, psize, pages, path, seen: vec![false; pages as usize], highest: 0 };
    for root in [meta.free, meta.main] {
        if let Some(missing) = walk.tree(root)? {
            return Ok(Reach { missing: Some(missing), highest: walk.highest });
        }
    }
    Ok(Reach { missing: None, highest: walk.highest })
}

/// The pages the main tree reaches, its sub-databases and overflow runs included, counted as the walk reaches them. Checks
/// the walk against LMDB's own counts of the trees in tests
#[cfg(test)]
pub(super) fn main_tree_pages(file: &mut File, psize: usize, pages: u64, txnid: u64, path: &Path) -> Result<usize> {
    let (_, main, _) =
        meta_of(file, psize, txnid).ok_or_else(|| refused(path, "holds no meta page of that transaction"))?;
    let mut walk = Walk { file, psize, pages, path, seen: vec![false; pages as usize], highest: 0 };
    if let Some(missing) = walk.tree(main)? {
        return Err(refused(path, &format!("reaches page {missing} past its end")));
    }
    Ok(walk.seen.iter().filter(|seen| **seen).count())
}

/// The meta page of the commit `snapshot` reads at, if the file holds it. Either meta page may be torn, and the two
/// are written in turn: the commit after the next writes over this one
pub(super) fn meta<'snapshot>(
    file: &mut File,
    psize: usize,
    snapshot: &'snapshot RoTxn<'_, WithoutTls>,
) -> Option<Meta<'snapshot>> {
    let (free, main, last_page) = meta_of(file, psize, snapshot.id() as u64)?;
    Some(Meta { free, main, last_page, _held: PhantomData })
}

/// The free tree's root, the main tree's and the last page, of the meta page transaction `txnid` wrote
fn meta_of(file: &mut File, psize: usize, txnid: u64) -> Option<(u64, u64, u64)> {
    for at in [0, psize as u64] {
        let Some(page) = read_at(file, at, META_LEN) else { continue };
        if u32_at(&page, META_MAGIC) != MAGIC || u64_at(&page, META_TXNID) != txnid {
            continue;
        }
        return Some((u64_at(&page, META_FREE_ROOT), u64_at(&page, META_MAIN_ROOT), u64_at(&page, META_LAST_PAGE)));
    }
    None
}

struct Walk<'a> {
    file: &'a mut File,
    psize: usize,
    pages: u64,
    path: &'a Path,
    seen: Vec<bool>,
    highest: u64,
}

impl Walk<'_> {
    /// Walks one tree from its root, and returns the first page it reaches past the end of the file
    fn tree(&mut self, root: u64) -> Result<Option<u64>> {
        let mut stack = vec![root];
        while let Some(pgno) = stack.pop() {
            if pgno == NONE {
                continue;
            }
            if pgno >= self.pages {
                return Ok(Some(pgno));
            }
            if std::mem::replace(&mut self.seen[pgno as usize], true) {
                continue;
            }
            self.highest = self.highest.max(pgno);
            let page = self.page(pgno)?;
            let flags = self.header(pgno, &page)?;
            if flags & P_OVERFLOW != 0 {
                return Err(corrupt(
                    self.path,
                    &format!("reaches page {pgno} as a tree page, which is an overflow page"),
                ));
            }
            for node in nodes(&page) {
                let (lo, hi, node_flags, key) = node_head(&page, node, self.path)?;
                let data = node + 8 + key;
                if flags & P_BRANCH != 0 {
                    stack.push(lo as u64 | (hi as u64) << 16 | (node_flags as u64) << 32);
                    continue;
                }
                if node_flags & F_DUPDATA != 0 {
                    return Err(refused(
                        self.path,
                        &format!(
                            "holds a duplicate-sorted node on page {pgno}, which elmdb never writes: it is another \
                             program's file, or one of elmdb's with that page torn by a power loss. Open a database \
                             elmdb wrote, or restore this one from a backup"
                        ),
                    ));
                }
                if node_flags & F_SUBDATA != 0 {
                    stack.push(u64_at_checked(&page, data + DB_ROOT, self.path)?);
                }
                if node_flags & F_BIGDATA != 0 {
                    let size = lo as usize | (hi as usize) << 16;
                    if let Some(missing) = self.overflow(u64_at_checked(&page, data, self.path)?, size)? {
                        return Ok(Some(missing));
                    }
                }
            }
        }
        Ok(None)
    }

    /// Marks the run of overflow pages that hold a value of `size` bytes, which begins at `pgno`
    fn overflow(&mut self, pgno: u64, size: usize) -> Result<Option<u64>> {
        if pgno >= self.pages {
            return Ok(Some(pgno));
        }
        let page = self.page(pgno)?;
        let flags = self.header(pgno, &page)?;
        if flags & P_OVERFLOW == 0 {
            return Err(corrupt(self.path, &format!("page {pgno} is a value's overflow page, and is not one")));
        }
        let count = u32_at_checked(&page, PAGE_HEADER - 4, self.path)? as u64;
        // LMDB's OVPAGES: the pages a value of this size takes, its header included
        let expected = ((PAGE_HEADER - 1 + size) / self.psize + 1) as u64;
        // LMDB overwrites a value in place, within the transaction that wrote its run, by a value that needs no more pages,
        // and keeps the run's count: a run may be longer than the value it holds, never shorter
        if count < expected {
            return Err(corrupt(
                self.path,
                &format!(
                    "the overflow run at page {pgno} is {count} pages, fewer than the {expected} a value of {size} bytes \
                     takes"
                ),
            ));
        }
        for page_no in pgno..pgno + count {
            if page_no >= self.pages {
                return Ok(Some(page_no));
            }
            self.seen[page_no as usize] = true;
            self.highest = self.highest.max(page_no);
        }
        Ok(None)
    }

    /// The page at `pgno`, read by a seek; the caller has checked that it is in the file
    fn page(&mut self, pgno: u64) -> Result<Vec<u8>> {
        let mut page = vec![0u8; self.psize];
        self.file.seek(SeekFrom::Start(pgno * self.psize as u64))?;
        self.file.read_exact(&mut page)?;
        Ok(page)
    }

    /// Checks that the page carries its own number and one of LMDB's page types, and returns its flags
    fn header(&self, pgno: u64, page: &[u8]) -> Result<u16> {
        let flags = u16_at_checked(page, 10, self.path)?;
        let kind = flags & (P_BRANCH | P_LEAF | P_OVERFLOW);
        let one_kind = kind.count_ones() == 1;
        if u64_at_checked(page, 0, self.path)? != pgno || !one_kind {
            return Err(corrupt(
                self.path,
                &format!("page {pgno} does not carry its own number and a tree page's kind"),
            ));
        }
        Ok(flags)
    }
}

/// The offsets of a page's nodes, from its pointer array, which ends at its `lower` bound
fn nodes(page: &[u8]) -> impl Iterator<Item = usize> + '_ {
    let lower = u16_at(page, 12) as usize;
    let count = lower.saturating_sub(PAGE_HEADER) / 2;
    (0..count).map(move |i| u16_at(page, PAGE_HEADER + 2 * i) as usize)
}

/// A node's low and high data-size words, its flags and its key size, read from the node's header
fn node_head(page: &[u8], node: usize, path: &Path) -> Result<(u16, u16, u16, usize)> {
    if node + 8 > page.len() {
        return Err(corrupt(path, "holds a node whose header runs past its page"));
    }
    Ok((u16_at(page, node), u16_at(page, node + 2), u16_at(page, node + 4), u16_at(page, node + 6) as usize))
}

fn read_at(file: &mut File, at: u64, len: usize) -> Option<Vec<u8>> {
    let mut bytes = vec![0u8; len];
    file.seek(SeekFrom::Start(at)).ok()?;
    file.read_exact(&mut bytes).ok()?;
    Some(bytes)
}

/// A page the walk can't read as LMDB wrote it, in the file's own words, and what there is to do about a torn file
fn corrupt(path: &Path, what: &str) -> Error {
    refused(
        path,
        &format!(
            "{what}: a power loss may have left it torn. Restore the database from a backup, or from a copy of the file \
             that is whole"
        ),
    )
}

/// A file refused as `CORRUPTED`: what is wrong with it and what to do, after its path
pub(super) fn refused(path: &Path, what: &str) -> Error {
    Error::new(ErrorCode::Corrupted, format!("{} {what}", path.display()))
}

fn u16_at(bytes: &[u8], at: usize) -> u16 {
    bytes.get(at..at + 2).map_or(0, |b| u16::from_ne_bytes([b[0], b[1]]))
}

fn u32_at(bytes: &[u8], at: usize) -> u32 {
    bytes.get(at..at + 4).map_or(0, |b| u32::from_ne_bytes([b[0], b[1], b[2], b[3]]))
}

fn u64_at(bytes: &[u8], at: usize) -> u64 {
    bytes.get(at..at + 8).map_or(0, |b| u64::from_ne_bytes(b.try_into().expect("eight bytes")))
}

fn u16_at_checked(bytes: &[u8], at: usize, path: &Path) -> Result<u16> {
    bytes
        .get(at..at + 2)
        .map(|b| u16::from_ne_bytes([b[0], b[1]]))
        .ok_or_else(|| corrupt(path, "has a field past its page"))
}

fn u32_at_checked(bytes: &[u8], at: usize, path: &Path) -> Result<u32> {
    bytes
        .get(at..at + 4)
        .map(|b| u32::from_ne_bytes([b[0], b[1], b[2], b[3]]))
        .ok_or_else(|| corrupt(path, "has a field past its page"))
}

fn u64_at_checked(bytes: &[u8], at: usize, path: &Path) -> Result<u64> {
    bytes
        .get(at..at + 8)
        .map(|b| u64::from_ne_bytes(b.try_into().expect("eight bytes")))
        .ok_or_else(|| corrupt(path, "has a field past its page"))
}
