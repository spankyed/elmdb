//! Bounded external sorting of derived entries. Scratch files are owned, never published, and deleted on drop.

use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::fs::{File, OpenOptions};
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

use crate::error::Result;

const MEMORY_BYTES: usize = 8 * 1024 * 1024;
const FAN_IN: usize = 16;

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

pub(super) struct Scratch {
    directory: tempfile::TempDir,
    runs: Vec<PathBuf>,
    rows: Vec<Row>,
    bytes: usize,
    next_order: u64,
    next_file: u64,
    budget: usize,
}

impl Scratch {
    pub(super) fn new() -> Result<Self> {
        Ok(Self {
            directory: tempfile::Builder::new().prefix("elmdb-check-").tempdir()?,
            runs: Vec::new(),
            rows: Vec::new(),
            bytes: 0,
            next_order: 0,
            next_file: 0,
            budget: MEMORY_BYTES,
        })
    }
    pub(super) fn push(&mut self, key: Vec<u8>, value: Vec<u8>) -> Result<()> {
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
        self.runs.push(path);
        Ok(())
    }
    pub(super) fn finish(mut self) -> Result<Sorted> {
        self.flush()?;
        if self.runs.is_empty() {
            let path = self.path();
            writer(&path)?.flush()?;
            self.runs.push(path);
        }
        // Always merge even one run: repeated keys must keep the last emitted value, as BTreeMap::insert did.
        loop {
            let runs = std::mem::take(&mut self.runs);
            for group in runs.chunks(FAN_IN) {
                let path = self.path();
                merge(group, &path)?;
                for old in group {
                    std::fs::remove_file(old)?;
                }
                self.runs.push(path);
            }
            if self.runs.len() == 1 {
                break;
            }
        }
        let reader = BufReader::new(File::open(&self.runs[0])?);
        Ok(Sorted { reader, _directory: self.directory })
    }
}

pub(super) struct Sorted {
    // Close the reader before deleting its directory, including on Windows.
    reader: BufReader<File>,
    _directory: tempfile::TempDir,
}
impl Sorted {
    pub(super) fn next(&mut self) -> Result<Option<(Vec<u8>, Vec<u8>)>> {
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
