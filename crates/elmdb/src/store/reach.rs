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
pub(super) const PAGE_HEADER: usize = 16;
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
const _: () = assert!(META_LEN - PAGE_HEADER == super::record::BODY, "a record holds a meta page's body whole");

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

/// The pages the trees from `roots` reach, in a file of whole pages of `psize` bytes, for a test that watches them
#[cfg(test)]
pub(super) fn pages_of(file: &mut File, psize: usize, roots: [u64; 2], path: &Path) -> Result<Vec<u64>> {
    let pages = file.metadata()?.len() / psize as u64;
    let mut walk = Walk { file, psize, pages, path, seen: vec![false; pages as usize], highest: 0 };
    for root in roots {
        if let Some(missing) = walk.tree(root)? {
            return Err(refused(path, &format!("reaches page {missing} past its end")));
        }
    }
    Ok(walk.seen.iter().enumerate().filter(|(_, seen)| **seen).map(|(at, _)| at as u64).collect())
}

/// The pages the free tree at `root` lists as freed by commits from `from` on: what a reader at `from` keeps from reuse,
/// LMDB giving a writer only what commits before the oldest reader freed. Each record is a commit's number and the
/// list of the pages it freed, whose first word is their count, on an overflow page where the list is long. Read by
/// seeks; a page that doesn't read as the tree's adds nothing
pub(super) fn freed_from(file: &mut File, psize: usize, root: u64, from: u64) -> u64 {
    let mut freed = 0u64;
    let mut stack = vec![root];
    // No tree holds more pages than the file: a cycle a torn page made ends there
    let mut budget = file.metadata().map_or(0, |m| m.len() / psize as u64);
    while let Some(pgno) = stack.pop() {
        if pgno == NONE || budget == 0 {
            continue;
        }
        budget -= 1;
        let Some(page) = pgno.checked_mul(psize as u64).and_then(|at| read_at(file, at, psize)) else { continue };
        let flags = u16_at(&page, 10);
        for node in nodes(&page).filter(|node| node + 8 <= page.len()) {
            let (lo, hi, node_flags) = (u16_at(&page, node), u16_at(&page, node + 2), u16_at(&page, node + 4));
            if flags & P_BRANCH != 0 {
                // The child's page number: its low, middle and high 16-bit words, the last in the node's flags
                let high = (node_flags as u64) << 32;
                // mutant-skip: replace \| with \^ in freed_from
                // The three words share no bit, so joined by `^` they make the same number
                stack.push(lo as u64 | (hi as u64) << 16 | high);
                continue;
            }
            if flags & P_LEAF == 0 || u64_at(&page, node + 8) < from {
                continue;
            }
            let data = node + 8 + u16_at(&page, node + 6) as usize;
            let count = if node_flags & F_BIGDATA != 0 {
                let at =
                    u64_at(&page, data).checked_mul(psize as u64).and_then(|at| at.checked_add(PAGE_HEADER as u64));
                at.and_then(|at| read_at(file, at, 8)).map_or(0, |b| u64_at(&b, 0))
            } else {
                u64_at(&page, data)
            };
            freed = freed.saturating_add(count);
        }
    }
    freed
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

/// One of the file's two meta pages as the file holds it: the commit it says it is, its body, and what the body names
pub(super) struct MetaPage {
    pub(super) txnid: u64,
    pub(super) body: [u8; super::record::BODY],
}

impl MetaPage {
    /// The roots of the free tree and the main tree
    pub(super) fn roots(&self) -> [u64; 2] {
        roots_of(&self.body)
    }

    pub(super) fn last_page(&self) -> u64 {
        u64_at(&self.body, META_LAST_PAGE - PAGE_HEADER)
    }
}

/// The roots of the free tree and the main tree a meta page's body names
pub(super) fn roots_of(body: &[u8; super::record::BODY]) -> [u64; 2] {
    [u64_at(body, META_FREE_ROOT - PAGE_HEADER), u64_at(body, META_MAIN_ROOT - PAGE_HEADER)]
}

/// The roots of the named databases a page of the main tree lists, where it is a leaf: the trees the main tree's
/// root reaches by name, elmdb's own among them
pub(super) fn named_roots(page: &[u8]) -> Vec<u64> {
    if u16_at(page, 10) & P_LEAF == 0 {
        return Vec::new();
    }
    nodes(page)
        .filter(|node| node + 8 <= page.len())
        .filter(|node| u16_at(page, node + 4) & F_SUBDATA != 0)
        .map(|node| u64_at(page, node + 8 + u16_at(page, node + 6) as usize + DB_ROOT))
        .collect()
}

/// The meta page in `slot`, 0 or 1, where it reads as one: LMDB's magic, and whole as far as a read of it goes
pub(super) fn meta_page(file: &mut File, psize: usize, slot: u64) -> Option<MetaPage> {
    let page = read_at(file, slot * psize as u64, META_LEN)?;
    if u32_at(&page, META_MAGIC) != MAGIC {
        return None;
    }
    let body = page[PAGE_HEADER..META_LEN].try_into().expect("a meta page's body");
    Some(MetaPage { txnid: u64_at(&page, META_TXNID), body })
}

/// The file's newer meta page, the one LMDB opens it at: of the higher commit, the first where both are of one
pub(super) fn newer_meta_page(file: &mut File, psize: usize) -> Option<MetaPage> {
    let (first, second) = (meta_page(file, psize, 0), meta_page(file, psize, 1));
    match (first, second) {
        (Some(first), Some(second)) => Some(if second.txnid > first.txnid { second } else { first }),
        (first, second) => first.or(second),
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A page of the main tree, a leaf or a branch by `flags`, with two nodes: one naming a database whose root is
    /// `root`, and one of a plain value whose bytes would read as the root 99
    fn page_with_a_named_root(flags: u16, root: u64) -> Vec<u8> {
        let mut page = vec![0u8; 4096];
        page[10..12].copy_from_slice(&flags.to_ne_bytes());
        // Two node pointers, then each node: its sizes, its flags, an empty key, and its data
        page[12..14].copy_from_slice(&((PAGE_HEADER + 4) as u16).to_ne_bytes());
        for (at, (node, node_flags, value)) in [(100usize, F_SUBDATA, root), (300, 0, 99)].into_iter().enumerate() {
            page[PAGE_HEADER + 2 * at..PAGE_HEADER + 2 * at + 2].copy_from_slice(&(node as u16).to_ne_bytes());
            page[node + 4..node + 6].copy_from_slice(&node_flags.to_ne_bytes());
            page[node + 8 + DB_ROOT..node + 8 + DB_ROOT + 8].copy_from_slice(&value.to_ne_bytes());
        }
        page
    }

    /// A page of `psize` bytes numbered `pgno`, of `flags`, with one node at `node` of `node_flags`, `key` and `data`;
    /// for a branch, `size` is the child's page number's low 32 bits, as LMDB keeps it in the node's size words
    fn page_with_a_node(
        psize: usize,
        pgno: u64,
        flags: u16,
        size: u32,
        node_flags: u16,
        key: &[u8],
        data: &[u8],
    ) -> Vec<u8> {
        let mut page = vec![0u8; psize];
        page[..8].copy_from_slice(&pgno.to_ne_bytes());
        page[10..12].copy_from_slice(&flags.to_ne_bytes());
        page[12..14].copy_from_slice(&((PAGE_HEADER + 2) as u16).to_ne_bytes());
        let node = 100usize;
        page[PAGE_HEADER..PAGE_HEADER + 2].copy_from_slice(&(node as u16).to_ne_bytes());
        page[node..node + 2].copy_from_slice(&(size as u16).to_ne_bytes());
        page[node + 2..node + 4].copy_from_slice(&((size >> 16) as u16).to_ne_bytes());
        page[node + 4..node + 6].copy_from_slice(&node_flags.to_ne_bytes());
        page[node + 6..node + 8].copy_from_slice(&(key.len() as u16).to_ne_bytes());
        page[node + 8..node + 8 + key.len()].copy_from_slice(key);
        page[node + 8 + key.len()..node + 8 + key.len() + data.len()].copy_from_slice(data);
        page
    }

    #[test]
    fn the_free_tree_is_read_through_a_branch_to_a_far_page_and_a_list_on_a_page_of_its_own() {
        // LMDB's smallest page, so a page past 2^16 of them is some 34 MB in: sparse where the filesystem makes it so,
        // and written out where it doesn't, as Windows' doesn't by default
        let psize = 512;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("free");
        let mut file =
            std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(true).open(&path).unwrap();
        // Past 2^16 pages, so the child's number has bits in the node's high size word
        let (leaf, list) = (65_537u64, 65_540u64);
        file.set_len((list + 1) * psize as u64).unwrap();
        let mut put = |pgno: u64, page: &[u8]| {
            std::io::Seek::seek(&mut file, std::io::SeekFrom::Start(pgno * psize as u64)).unwrap();
            std::io::Write::write_all(&mut file, page).unwrap();
        };
        put(0, &page_with_a_node(psize, 0, P_BRANCH, leaf as u32, 0, &[], &[]));
        // Two records: commit 5's list inline, seven pages, and commit 9's on its own page, forty-two
        let mut leaf_page = page_with_a_node(
            psize,
            leaf,
            P_LEAF,
            16,
            0,
            &5u64.to_ne_bytes(),
            &[7u64.to_ne_bytes(), 1u64.to_ne_bytes()].concat(),
        );
        let second = 300usize;
        leaf_page[12..14].copy_from_slice(&((PAGE_HEADER + 4) as u16).to_ne_bytes());
        leaf_page[PAGE_HEADER + 2..PAGE_HEADER + 4].copy_from_slice(&(second as u16).to_ne_bytes());
        leaf_page[second..second + 2].copy_from_slice(&8u16.to_ne_bytes());
        leaf_page[second + 4..second + 6].copy_from_slice(&F_BIGDATA.to_ne_bytes());
        leaf_page[second + 6..second + 8].copy_from_slice(&8u16.to_ne_bytes());
        leaf_page[second + 8..second + 16].copy_from_slice(&9u64.to_ne_bytes());
        leaf_page[second + 16..second + 24].copy_from_slice(&list.to_ne_bytes());
        put(leaf, &leaf_page);
        let mut list_page = vec![0u8; psize];
        list_page[..8].copy_from_slice(&list.to_ne_bytes());
        list_page[PAGE_HEADER..PAGE_HEADER + 8].copy_from_slice(&42u64.to_ne_bytes());
        put(list, &list_page);
        // A branch whose child's number has its high word set, the leaf's low words below it: a page past the file,
        // which holds nothing, and not the leaf
        let mut far = page_with_a_node(psize, 1, P_BRANCH, leaf as u32, 0, &[], &[]);
        far[100 + 4..100 + 6].copy_from_slice(&1u16.to_ne_bytes());
        put(1, &far);
        assert_eq!(freed_from(&mut file, psize, 0, 0), 49);
        assert_eq!(freed_from(&mut file, psize, 0, 6), 42, "from commit 6 on, the list on its own page alone");
        assert_eq!(freed_from(&mut file, psize, 0, 10), 0);
        assert_eq!(freed_from(&mut file, psize, 1, 0), 0, "the child past 2^32 pages, which the file doesn't hold");
    }

    /// A file of `pages`, each at its number, and the directory that holds it
    fn file_of(pages: &[Vec<u8>]) -> (tempfile::TempDir, File) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("free");
        std::fs::write(&path, pages.concat()).unwrap();
        let file = File::open(&path).unwrap();
        (dir, file)
    }

    /// A branch page whose nodes are at the offsets given, each naming the child its low word holds
    fn branch(psize: usize, nodes: &[(usize, u16)]) -> Vec<u8> {
        let mut page = vec![0u8; psize];
        page[10..12].copy_from_slice(&P_BRANCH.to_ne_bytes());
        page[12..14].copy_from_slice(&((PAGE_HEADER + 2 * nodes.len()) as u16).to_ne_bytes());
        for (i, (at, child)) in nodes.iter().enumerate() {
            page[PAGE_HEADER + 2 * i..PAGE_HEADER + 2 * i + 2].copy_from_slice(&(*at as u16).to_ne_bytes());
            page[*at..*at + 2].copy_from_slice(&child.to_ne_bytes());
        }
        page
    }

    /// A leaf at page 2 holding commit 5's record of seven pages freed
    fn leaf_of_seven(psize: usize) -> Vec<u8> {
        let list = [7u64.to_ne_bytes(), 1u64.to_ne_bytes()].concat();
        page_with_a_node(psize, 2, P_LEAF, 16, 0, &5u64.to_ne_bytes(), &list)
    }

    #[test]
    fn a_free_tree_a_torn_page_made_a_cycle_of_is_read_no_further_than_the_file_has_pages() {
        let psize = 512;
        // Page 1 a branch naming itself and the leaf: the stack holds two pages at most, and each turn of the cycle
        // reads the branch and the leaf
        let pages = vec![vec![0u8; psize], branch(psize, &[(100, 1), (200, 2)]), leaf_of_seven(psize)];
        // On a thread of its own and waited for a while: a budget that never ran out would read the cycle for good
        let (answered, answer) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let (_dir, mut file) = file_of(&pages);
            let _ = answered.send(freed_from(&mut file, psize, 1, 0));
        });
        // Three pages read, the file's: the branch, the leaf, the branch again; the leaf once
        let freed = answer.recv_timeout(std::time::Duration::from_secs(10)).expect("the cycle was read without end");
        assert_eq!(freed, 7);
    }

    #[test]
    fn a_page_that_is_neither_branch_nor_leaf_names_nothing_freed() {
        let psize = 512;
        // The leaf's node and record, on a page whose flags say an overflow page, as a torn tree's may
        let mut overflow = leaf_of_seven(psize);
        overflow[10..12].copy_from_slice(&P_OVERFLOW.to_ne_bytes());
        let (_dir, mut file) = file_of(&[vec![0u8; psize], vec![0u8; psize], overflow]);
        assert_eq!(freed_from(&mut file, psize, 2, 0), 0);
        let (_dir, mut file) = file_of(&[vec![0u8; psize], vec![0u8; psize], leaf_of_seven(psize)]);
        assert_eq!(freed_from(&mut file, psize, 2, 0), 7, "the same page as a leaf");
    }

    #[test]
    fn a_branch_node_whose_head_runs_past_its_page_names_no_child() {
        let psize = 512;
        // Its pointer six bytes before the page's end, where its low word, within the page, names the leaf
        let pages = vec![vec![0u8; psize], branch(psize, &[(psize - 6, 2)]), leaf_of_seven(psize)];
        let (_dir, mut file) = file_of(&pages);
        assert_eq!(freed_from(&mut file, psize, 1, 0), 0);
    }

    #[test]
    fn a_leaf_of_the_main_tree_names_its_databases_roots_and_a_branch_names_none() {
        assert_eq!(named_roots(&page_with_a_named_root(P_LEAF, 42)), vec![42], "a plain value is no database");
        assert_eq!(named_roots(&page_with_a_named_root(P_BRANCH, 42)), Vec::<u64>::new());
    }

    #[test]
    fn a_node_whose_head_runs_past_its_page_names_no_root() {
        // The plain value's node moved to six bytes before the page's end: its flags, within the page, name a
        // database, and its root would be read past the page
        let mut page = page_with_a_named_root(P_LEAF, 42);
        let at = page.len() - 6;
        page[PAGE_HEADER + 2..PAGE_HEADER + 4].copy_from_slice(&(at as u16).to_ne_bytes());
        page[at + 4..at + 6].copy_from_slice(&F_SUBDATA.to_ne_bytes());
        assert_eq!(named_roots(&page), vec![42]);
    }

    #[test]
    fn the_newer_meta_page_is_of_the_higher_commit_and_the_first_where_both_are_of_one() {
        use std::io::Write;
        const PSIZE: usize = 4096;
        // A meta page of `txnid`, its body's bytes past the magic all `mark`
        let meta = |txnid: u64, mark: u8| {
            let mut page = vec![mark; PSIZE];
            page[META_MAGIC..META_MAGIC + 4].copy_from_slice(&MAGIC.to_ne_bytes());
            page[META_TXNID..META_TXNID + 8].copy_from_slice(&txnid.to_ne_bytes());
            page
        };
        let newer = |first: Vec<u8>, second: Vec<u8>| {
            let mut file = tempfile::tempfile().unwrap();
            file.write_all(&[first, second].concat()).unwrap();
            newer_meta_page(&mut file, PSIZE).map(|meta| (meta.txnid, meta.body[4]))
        };
        assert_eq!(newer(meta(5, 1), meta(5, 2)), Some((5, 1)), "of one commit");
        assert_eq!(newer(meta(5, 1), meta(6, 2)), Some((6, 2)));
        assert_eq!(newer(meta(7, 1), meta(6, 2)), Some((7, 1)));
        assert_eq!(newer(vec![0; PSIZE], meta(6, 2)), Some((6, 2)), "the first no meta page");
    }
}
