//! Bounded external sorting of derived entries. Scratch files are owned, never published, and deleted on drop.

use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::fs::{File, OpenOptions};
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

use crate::diagnostics::{Op, names};
use crate::error::Result;

const MEMORY_BYTES: usize = 8 * 1024 * 1024;
const FAN_IN: usize = 16;

#[cfg(test)]
thread_local! {
    pub(in crate::engine) static TEST_BUDGET: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
}

#[derive(PartialEq, Eq)]
struct Row {
    key: Vec<u8>,
    value: Vec<u8>,
    order: u64,
}

#[derive(PartialEq, Eq)]
struct Head {
    row: Row,
    run: usize,
}

impl Ord for Head {
    fn cmp(&self, other: &Self) -> Ordering {
        other
            .row
            .key
            .cmp(&self.row.key)
            .then_with(|| other.row.order.cmp(&self.row.order))
            .then_with(|| other.row.value.cmp(&self.row.value))
            .then_with(|| other.run.cmp(&self.run))
    }
}
impl PartialOrd for Head {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

pub(in crate::engine) struct Scratch {
    directory: tempfile::TempDir,
    runs: Vec<(PathBuf, usize)>,
    rows: Vec<Row>,
    bytes: usize,
    next_order: u64,
    next_file: u64,
    budget: usize,
    op: Op,
}

impl Scratch {
    pub(in crate::engine) fn new() -> Result<Self> {
        failure_point!(SCRATCH, std::io::Error::other("injected scratch creation failure").into());
        Ok(Self {
            directory: tempfile::Builder::new().prefix("elmdb-check-").tempdir()?,
            runs: Vec::new(),
            rows: Vec::new(),
            bytes: 0,
            next_order: 0,
            next_file: 0,
            op: Op::sort(),
            budget: {
                #[cfg(test)]
                {
                    TEST_BUDGET.get().unwrap_or(MEMORY_BYTES)
                }
                #[cfg(not(test))]
                {
                    MEMORY_BYTES
                }
            },
        })
    }
    pub(in crate::engine) fn push(&mut self, key: Vec<u8>, value: Vec<u8>) -> Result<()> {
        self.bytes = self
            .bytes
            .saturating_add(key.capacity())
            .saturating_add(value.capacity())
            .saturating_add(std::mem::size_of::<Row>());
        self.rows.push(Row { key, value, order: self.next_order });
        self.next_order += 1;
        if self.bytes >= self.budget {
            self.flush()?;
        }
        Ok(())
    }
    fn path(&mut self) -> PathBuf {
        let path = self.directory.path().join(format!("run-{}", self.next_file));
        self.next_file += 1;
        path
    }
    fn flush(&mut self) -> Result<()> {
        if self.rows.is_empty() {
            return Ok(());
        }
        self.rows.sort_unstable_by(|a, b| a.key.cmp(&b.key).then(a.order.cmp(&b.order)));
        let path = self.path();
        let mut file = writer(&path)?;
        for row in self.rows.drain(..) {
            write_row(&mut file, &row)?;
        }
        file.flush()?;
        self.bytes = 0;
        self.runs.push((path, 0));
        // Compact equal-sized levels as runs arrive. At most FAN_IN-1 runs per level remain; retaining every
        // original path would itself grow with the dataset. Each row participates in O(log runs) merges.
        while let Some((_, level)) = self.runs.last() {
            let level = *level;
            let start = self.runs.len().saturating_sub(FAN_IN);
            if self.runs.len() < FAN_IN || self.runs[start..].iter().any(|(_, at)| *at != level) {
                break;
            }
            let paths: Vec<_> = self.runs.drain(start..).map(|(path, _)| path).collect();
            let target = self.path();
            merge(&paths, &target)?;
            for path in paths {
                std::fs::remove_file(path)?;
            }
            self.runs.push((target, level + 1));
        }
        Ok(())
    }
    pub(in crate::engine) fn finish(mut self) -> Result<Sorted> {
        let op = std::mem::replace(&mut self.op, Op::none());
        let rows = self.next_order;
        let sorted = self.sorted(&op);
        op.record(names::ROWS, || rows);
        op.ended(sorted, "sorted")
    }

    fn sorted(mut self, op: &Op) -> Result<Sorted> {
        self.flush()?;
        op.record(names::RUNS, || self.next_file);
        op.record(names::SPILLED_BYTES, || {
            self.runs.iter().map(|(path, _)| std::fs::metadata(path).map_or(0, |m| m.len())).sum()
        });
        if self.runs.is_empty() {
            let path = self.path();
            writer(&path)?.flush()?;
            self.runs.push((path, 0));
        }
        // Always merge even one run: repeated keys must keep the last emitted value, as BTreeMap::insert did.
        loop {
            let runs = std::mem::take(&mut self.runs);
            for group in runs.chunks(FAN_IN) {
                let path = self.path();
                let paths: Vec<_> = group.iter().map(|(path, _)| path.clone()).collect();
                merge(&paths, &path)?;
                for old in paths {
                    std::fs::remove_file(old)?;
                }
                self.runs.push((path, 0));
            }
            if self.runs.len() == 1 {
                break;
            }
        }
        let reader = BufReader::new(File::open(&self.runs[0].0)?);
        Ok(Sorted { reader, _directory: self.directory })
    }
}

pub(in crate::engine) struct Sorted {
    // Close the reader before deleting its directory, including on Windows.
    reader: BufReader<File>,
    _directory: tempfile::TempDir,
}
impl Sorted {
    pub(in crate::engine) fn next(&mut self) -> Result<Option<(Vec<u8>, Vec<u8>)>> {
        Ok(read_row(&mut self.reader)?.map(|row| (row.key, row.value)))
    }
}

fn writer(path: &Path) -> Result<BufWriter<File>> {
    Ok(BufWriter::new(OpenOptions::new().create_new(true).write(true).open(path)?))
}
fn write_row(file: &mut impl Write, row: &Row) -> Result<()> {
    for n in [row.order, row.key.len() as u64, row.value.len() as u64] {
        file.write_all(&n.to_le_bytes())?;
    }
    file.write_all(&row.key)?;
    file.write_all(&row.value)?;
    Ok(())
}
fn read_row(file: &mut impl Read) -> Result<Option<Row>> {
    let mut head = [0u8; 24];
    if file.read(&mut head[..1])? == 0 {
        return Ok(None);
    }
    file.read_exact(&mut head[1..])?;
    let number = |at| u64::from_le_bytes(head[at..at + 8].try_into().expect("eight bytes"));
    let mut bytes = |n| -> Result<Vec<u8>> {
        let mut bytes = Vec::new();
        (&mut *file).take(n).read_to_end(&mut bytes)?;
        if bytes.len() as u64 != n {
            return Err(std::io::Error::from(std::io::ErrorKind::UnexpectedEof).into());
        }
        Ok(bytes)
    };
    Ok(Some(Row { order: number(0), key: bytes(number(8))?, value: bytes(number(16))? }))
}
fn merge(paths: &[PathBuf], target: &Path) -> Result<()> {
    let mut readers =
        paths.iter().map(|path| File::open(path).map(BufReader::new)).collect::<std::io::Result<Vec<_>>>()?;
    let mut heap = BinaryHeap::new();
    for (run, reader) in readers.iter_mut().enumerate() {
        if let Some(row) = read_row(reader)? {
            heap.push(Head { row, run });
        }
    }
    let mut out = writer(target)?;
    let mut pending: Option<Row> = None;
    while let Some(Head { row, run }) = heap.pop() {
        if let Some(next) = read_row(&mut readers[run])? {
            heap.push(Head { row: next, run });
        }
        if let Some(previous) = pending.take()
            && previous.key != row.key
        {
            write_row(&mut out, &previous)?;
        }
        pending = Some(row);
    }
    if let Some(row) = pending {
        write_row(&mut out, &row)?;
    }
    out.flush()?;
    Ok(())
}

#[cfg(test)]
pub(in crate::engine) fn with_budget<T>(bytes: usize, run: impl FnOnce() -> T) -> T {
    struct Restore(Option<usize>);
    impl Drop for Restore {
        fn drop(&mut self) {
            TEST_BUDGET.set(self.0);
        }
    }
    let _restore = Restore(TEST_BUDGET.replace(Some(bytes)));
    run()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    #[test]
    fn spilled_and_merged_runs_match_last_insert_wins_and_clean_up() {
        let mut scratch = Scratch::new().unwrap();
        scratch.budget = 64;
        let path = scratch.directory.path().to_owned();
        let mut expected = BTreeMap::new();
        for (order, i) in (0..300u64).rev().chain(0..300).enumerate() {
            let key = (i % 100).to_be_bytes().to_vec();
            let value = (order as u64).to_be_bytes().to_vec();
            expected.insert(key.clone(), value.clone());
            scratch.push(key, value).unwrap();
        }
        let mut sorted = scratch.finish().unwrap();
        for row in expected {
            assert_eq!(sorted.next().unwrap(), Some(row));
        }
        assert!(sorted.next().unwrap().is_none());
        drop(sorted);
        assert!(!path.exists());
    }
    #[test]
    fn the_last_of_several_rows_pushed_under_one_key_wins_within_a_run() {
        let mut scratch = Scratch::new().unwrap();
        let mut expected = BTreeMap::new();
        for i in 0..10_000u64 {
            let key = (i % 10).to_be_bytes().to_vec();
            let value = i.to_be_bytes().to_vec();
            expected.insert(key.clone(), value.clone());
            scratch.push(key, value).unwrap();
        }
        assert!(scratch.runs.is_empty(), "nothing spilled");
        let mut sorted = scratch.finish().unwrap();
        for row in expected {
            assert_eq!(sorted.next().unwrap(), Some(row));
        }
        assert!(sorted.next().unwrap().is_none());
    }
    #[test]
    fn runs_merge_by_level_sixteen_of_one_into_one_of_the_next() {
        let mut scratch = Scratch::new().unwrap();
        scratch.budget = 1;
        for i in 0..(FAN_IN * FAN_IN) as u64 {
            scratch.push(i.to_be_bytes().to_vec(), Vec::new()).unwrap();
        }
        let levels: Vec<usize> = scratch.runs.iter().map(|(_, level)| *level).collect();
        assert_eq!(levels, [2]);
    }
    #[test]
    fn incoming_runs_compact_without_retaining_every_path() {
        let mut scratch = Scratch::new().unwrap();
        scratch.budget = 1;
        let mut expected = BTreeMap::new();
        for i in 0..1024u64 {
            let key = (i % 71).to_be_bytes().to_vec();
            let value = i.to_be_bytes().to_vec();
            expected.insert(key.clone(), value.clone());
            scratch.push(key, value).unwrap();
            assert!(scratch.runs.len() <= 45, "path metadata must compact while accepting rows");
        }
        let mut sorted = scratch.finish().unwrap();
        for row in expected {
            assert_eq!(sorted.next().unwrap(), Some(row));
        }
        assert!(sorted.next().unwrap().is_none());
    }

    #[test]
    fn an_intermediate_merge_failure_removes_its_private_files() {
        let mut scratch = Scratch::new().unwrap();
        scratch.budget = 1;
        let path = scratch.directory.path().to_owned();
        File::create(path.join("run-16")).unwrap();
        for i in 0..15u64 {
            scratch.push(i.to_be_bytes().to_vec(), vec![]).unwrap();
        }
        assert!(scratch.push(15u64.to_be_bytes().to_vec(), vec![]).is_err());
        drop(scratch);
        assert!(!path.exists());
    }

    #[test]
    fn interrupted_or_failed_sorting_removes_owned_scratch() {
        let mut scratch = Scratch::new().unwrap();
        scratch.budget = 1;
        let path = scratch.directory.path().to_owned();
        File::create(path.join("run-0")).unwrap();
        assert!(scratch.push(b"key".to_vec(), b"value".to_vec()).is_err());
        drop(scratch);
        assert!(!path.exists());
        let mut scratch = Scratch::new().unwrap();
        scratch.budget = 1;
        let path = scratch.directory.path().to_owned();
        scratch.push(b"key".to_vec(), b"value".to_vec()).unwrap();
        drop(scratch);
        assert!(!path.exists());
    }

    #[test]
    fn a_truncated_frame_is_an_error_instead_of_an_end_marker() {
        let mut bytes = Vec::new();
        write_row(&mut bytes, &Row { key: b"key".to_vec(), value: b"value".to_vec(), order: 0 }).unwrap();
        bytes.pop();
        assert!(read_row(&mut bytes.as_slice()).is_err());
    }

    #[test]
    fn empty_and_larger_than_budget_rows_are_preserved() {
        assert!(Scratch::new().unwrap().finish().unwrap().next().unwrap().is_none());
        let mut scratch = Scratch::new().unwrap();
        scratch.budget = 8;
        scratch.push(b"key".to_vec(), vec![7; 100000]).unwrap();
        let mut sorted = scratch.finish().unwrap();
        assert_eq!(sorted.next().unwrap(), Some((b"key".to_vec(), vec![7; 100000])));
        assert!(sorted.next().unwrap().is_none());
    }
}
