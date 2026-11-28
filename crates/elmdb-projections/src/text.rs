//! Full-Text BM25F projection with Block-Max WAND (BMW) inside a single sub-DB.
//!
//! Table tags inside the index's pooled `SubDbSlot`:
//! - `0x00` -> `TextStats { doc_count: u64, total_weighted_len: u64, field_len_totals: Vec<u64> }`
//! - `0x01 || term_utf8` -> `TermDictEntry { doc_freq: u32, max_weighted_tf: u32, min_weighted_doc_len: u64 }`
//! - `0x02 || term_utf8 || 0x00 || bucket(u64 BE)` -> `BlockMaxHeader(max_weighted_tf: u32, min_weighted_doc_len: u64, count: u16)` + delta-varint postings
//! - `0x03 || EntityKey(8B BE)` -> bounded-zstd binary `StoredDoc` (positions for phrase match & exact incremental delta)
//! - `0x04 || bucket(u64 BE)` -> compact `(offset: u8, weighted_doc_len: u64)` table for O(1) length lookup

use std::cmp::{Ordering, Reverse};
use std::collections::{BTreeMap, BinaryHeap};
use std::io::Read as _;

use charabia::Tokenize;
use elmdb_engine::{EntityKey, FieldId, PackedRow};
use elmdb_storage::{KvCursor, KvRead, SubDbHandle};
use elmdb_wire::{CanonicalValue, ElmdbError, ErrorCode, Result};
use rust_stemmers::{Algorithm, Stemmer};
use rustc_hash::{FxHashMap, FxHashSet};

use crate::state::KvMut;

const TAG_STATS: u8 = 0x00;
const TAG_DICT: u8 = 0x01;
const TAG_BLOCK: u8 = 0x02;
const TAG_DOCS: u8 = 0x03;
const TAG_LENGTHS: u8 = 0x04;

const DOC_RAW: u8 = 0x00;
const DOC_ZSTD: u8 = 0x01;
const DOC_ZSTD_THRESHOLD: usize = 128;
const MAX_DOC_BYTES: usize = 32 * 1024 * 1024;

/// Position gap inserted between elements of a list-of-string field so phrases never span elements.
pub const ELEMENT_GAP: u32 = 64;
/// Maximum UTF-8 byte length of an indexed term.
pub const MAX_TERM_BYTES: usize = 128;
/// Entities per posting block (`EntityKey >> 7`, 128-entity aligned buckets).
pub const BLOCK_SHIFT: u32 = 7;

const BM25_K1: f64 = 1.2;
const BM25_B: f64 = 0.75;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnalyzedToken {
    pub term: String,
    pub position: u32,
    pub byte_start: usize,
    pub byte_end: usize,
}

/// Tokenizes text using `charabia` (with camelCase / snake_case splitting and code preservation
/// such as `THR-12` alongside its components) and optional Snowball stemming.
pub fn analyze_text(text: &str, stem_lang: Option<&str>) -> Vec<AnalyzedToken> {
    let stemmer = stem_lang.and_then(stem_algorithm).map(Stemmer::create);
    let mut words: Vec<AnalyzedToken> = Vec::new();
    for token in text.tokenize() {
        if !token.is_word() || token.lemma().is_empty() {
            continue;
        }
        words.push(AnalyzedToken {
            term: token.lemma().to_owned(),
            position: words.len() as u32,
            byte_start: token.byte_start,
            byte_end: token.byte_end,
        });
    }
    let mut out = Vec::with_capacity(words.len());
    for (idx, w) in words.iter().enumerate() {
        let stemmed = match &stemmer {
            Some(s) => s.stem(&w.term).into_owned(),
            None => w.term.clone(),
        };
        if stemmed.len() <= MAX_TERM_BYTES {
            out.push(AnalyzedToken {
                term: stemmed,
                position: w.position,
                byte_start: w.byte_start,
                byte_end: w.byte_end,
            });
        }
        if let Some(next) = words.get(idx + 1)
            && text.get(w.byte_end..next.byte_start) == Some("-")
            && w.term.chars().all(char::is_alphabetic)
            && next.term.chars().all(|c| c.is_ascii_digit())
        {
            let code = format!("{}-{}", w.term, next.term);
            if code.len() <= MAX_TERM_BYTES {
                out.push(AnalyzedToken {
                    term: code,
                    position: w.position,
                    byte_start: w.byte_start,
                    byte_end: next.byte_end,
                });
            }
        }
    }
    out
}

fn stem_algorithm(lang: &str) -> Option<Algorithm> {
    Some(match lang {
        "ar" => Algorithm::Arabic,
        "da" => Algorithm::Danish,
        "de" => Algorithm::German,
        "el" => Algorithm::Greek,
        "en" => Algorithm::English,
        "es" => Algorithm::Spanish,
        "fi" => Algorithm::Finnish,
        "fr" => Algorithm::French,
        "hu" => Algorithm::Hungarian,
        "it" => Algorithm::Italian,
        "nl" => Algorithm::Dutch,
        "no" => Algorithm::Norwegian,
        "pt" => Algorithm::Portuguese,
        "ro" => Algorithm::Romanian,
        "ru" => Algorithm::Russian,
        "sv" => Algorithm::Swedish,
        "ta" => Algorithm::Tamil,
        "tr" => Algorithm::Turkish,
        _ => return None,
    })
}

/// Single-pass `O(L + S log S)` UTF-8 byte-span to UTF-16 code-unit span mapper for highlights.
/// Eliminates v1's `O(S * L)` quadratic UTF-16 counting loop.
pub fn map_utf8_spans_to_utf16(text: &str, byte_spans: &[(usize, usize)]) -> Vec<(usize, usize)> {
    if byte_spans.is_empty() {
        return Vec::new();
    }
    // Collect all unique boundary byte offsets we need mapped.
    let mut offsets: Vec<usize> = Vec::with_capacity(byte_spans.len() * 2);
    for &(s, e) in byte_spans {
        offsets.push(s.min(text.len()));
        offsets.push(e.min(text.len()));
    }
    offsets.sort_unstable();
    offsets.dedup();

    let mut mapped: FxHashMap<usize, usize> = FxHashMap::default();
    mapped.reserve(offsets.len());

    let mut cur_utf16 = 0usize;
    let mut off_idx = 0usize;
    for (byte_pos, ch) in text.char_indices() {
        while off_idx < offsets.len() && offsets[off_idx] <= byte_pos {
            mapped.insert(offsets[off_idx], cur_utf16);
            off_idx += 1;
        }
        cur_utf16 += ch.len_utf16();
    }
    while off_idx < offsets.len() {
        mapped.insert(offsets[off_idx], cur_utf16);
        off_idx += 1;
    }

    byte_spans
        .iter()
        .map(|&(s, e)| {
            let s16 = *mapped.get(&s.min(text.len())).unwrap_or(&cur_utf16);
            let e16 = *mapped.get(&e.min(text.len())).unwrap_or(&cur_utf16);
            (s16, e16)
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Compact binary codecs for stats, dict, blocks, docs, lengths
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextStats {
    pub doc_count: u64,
    pub total_weighted_len: u64,
    pub field_len_totals: Vec<u64>,
}

impl TextStats {
    fn empty(field_count: usize) -> Self {
        Self { doc_count: 0, total_weighted_len: 0, field_len_totals: vec![0; field_count] }
    }

    fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(18 + self.field_len_totals.len() * 8);
        out.extend_from_slice(&self.doc_count.to_le_bytes());
        out.extend_from_slice(&self.total_weighted_len.to_le_bytes());
        out.extend_from_slice(&(self.field_len_totals.len() as u16).to_le_bytes());
        for &l in &self.field_len_totals {
            out.extend_from_slice(&l.to_le_bytes());
        }
        out
    }

    fn decode(raw: &[u8]) -> Result<Self> {
        if raw.len() < 18 {
            return Err(ElmdbError::new(ErrorCode::Corrupted, "truncated text stats"));
        }
        let doc_count = u64::from_le_bytes(raw[0..8].try_into().expect("8"));
        let total_weighted_len = u64::from_le_bytes(raw[8..16].try_into().expect("8"));
        let n = u16::from_le_bytes([raw[16], raw[17]]) as usize;
        if raw.len() != 18 + n * 8 {
            return Err(ElmdbError::new(ErrorCode::Corrupted, "corrupt text stats length"));
        }
        let mut field_len_totals = Vec::with_capacity(n);
        for i in 0..n {
            let off = 18 + i * 8;
            field_len_totals.push(u64::from_le_bytes(raw[off..off + 8].try_into().expect("8")));
        }
        Ok(Self { doc_count, total_weighted_len, field_len_totals })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TermDictEntry {
    pub doc_freq: u32,
    pub max_weighted_tf: u32,
    pub min_weighted_doc_len: u64,
}

impl TermDictEntry {
    fn encode(self) -> [u8; 16] {
        let mut buf = [0u8; 16];
        buf[0..4].copy_from_slice(&self.doc_freq.to_le_bytes());
        buf[4..8].copy_from_slice(&self.max_weighted_tf.to_le_bytes());
        buf[8..16].copy_from_slice(&self.min_weighted_doc_len.to_le_bytes());
        buf
    }

    fn decode(raw: &[u8]) -> Result<Self> {
        if raw.len() != 16 {
            return Err(ElmdbError::new(ErrorCode::Corrupted, "corrupt term dict entry"));
        }
        Ok(Self {
            doc_freq: u32::from_le_bytes(raw[0..4].try_into().expect("4")),
            max_weighted_tf: u32::from_le_bytes(raw[4..8].try_into().expect("4")),
            min_weighted_doc_len: u64::from_le_bytes(raw[8..16].try_into().expect("8")),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Posting {
    offset: u8, // 0..128 within bucket
    weighted_tf: u32,
    weighted_doc_len: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PostingBlock {
    max_weighted_tf: u32,
    min_weighted_doc_len: u64,
    postings: Vec<Posting>,
}

impl PostingBlock {
    fn recompute_bounds(&mut self) {
        self.max_weighted_tf = 0;
        self.min_weighted_doc_len = u64::MAX;
        for p in &self.postings {
            self.max_weighted_tf = self.max_weighted_tf.max(p.weighted_tf);
            self.min_weighted_doc_len = self.min_weighted_doc_len.min(p.weighted_doc_len);
        }
    }

    fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(14 + self.postings.len() * 6);
        out.extend_from_slice(&self.max_weighted_tf.to_le_bytes());
        out.extend_from_slice(&self.min_weighted_doc_len.to_le_bytes());
        out.extend_from_slice(&(self.postings.len() as u16).to_le_bytes());
        let mut prev_off = 0u8;
        for p in &self.postings {
            let delta = p.offset - prev_off;
            prev_off = p.offset;
            out.push(delta);
            write_varint(p.weighted_tf as u64, &mut out);
            write_varint(p.weighted_doc_len, &mut out);
        }
        out
    }

    fn decode_header(raw: &[u8]) -> Result<(u32, u64, u16)> {
        if raw.len() < 14 {
            return Err(ElmdbError::new(ErrorCode::Corrupted, "truncated posting block header"));
        }
        let max_tf = u32::from_le_bytes(raw[0..4].try_into().expect("4"));
        let min_len = u64::from_le_bytes(raw[4..12].try_into().expect("8"));
        let count = u16::from_le_bytes([raw[12], raw[13]]);
        Ok((max_tf, min_len, count))
    }

    fn decode(raw: &[u8]) -> Result<Self> {
        let (max_weighted_tf, min_weighted_doc_len, count) = Self::decode_header(raw)?;
        let max_per_block = 1usize << BLOCK_SHIFT;
        if (count as usize) > max_per_block {
            return Err(ElmdbError::new(ErrorCode::Corrupted, "posting block count exceeds block capacity"));
        }
        let mut pos = 14usize;
        let mut postings = Vec::with_capacity(count as usize);
        let mut cur_off = 0u8;
        for i in 0..count {
            if pos >= raw.len() {
                return Err(ElmdbError::new(ErrorCode::Corrupted, "truncated posting block body"));
            }
            let delta = raw[pos];
            if i > 0 && delta == 0 {
                return Err(ElmdbError::new(ErrorCode::Corrupted, "non-monotonic posting offset delta"));
            }
            cur_off = cur_off
                .checked_add(delta)
                .ok_or_else(|| ElmdbError::new(ErrorCode::Corrupted, "posting offset overflow"))?;
            if (cur_off as usize) >= max_per_block {
                return Err(ElmdbError::new(ErrorCode::Corrupted, "posting offset out of block bounds"));
            }
            pos += 1;
            let tf = read_varint(raw, &mut pos)? as u32;
            let doc_len = read_varint(raw, &mut pos)?;
            postings.push(Posting { offset: cur_off, weighted_tf: tf, weighted_doc_len: doc_len });
        }
        if pos != raw.len() {
            return Err(ElmdbError::new(ErrorCode::Corrupted, "trailing bytes in posting block"));
        }
        Ok(Self { max_weighted_tf, min_weighted_doc_len, postings })
    }
}

type TermFieldPositions = Vec<(u8, Vec<u32>)>;

#[derive(Debug, Clone, PartialEq, Eq)]
struct StoredDoc {
    weighted_doc_len: u64,
    field_lens: Vec<u64>,
    /// Sorted by term: `(term, weighted_tf, positions_by_field)`.
    terms: Vec<(String, u32, TermFieldPositions)>,
}

impl StoredDoc {
    fn encode(&self) -> Result<Vec<u8>> {
        let mut raw = Vec::with_capacity(64 + self.terms.len() * 16);
        write_varint(self.weighted_doc_len, &mut raw);
        write_varint(self.field_lens.len() as u64, &mut raw);
        for &fl in &self.field_lens {
            write_varint(fl, &mut raw);
        }
        write_varint(self.terms.len() as u64, &mut raw);
        for (term, wtf, fpos) in &self.terms {
            write_varint(term.len() as u64, &mut raw);
            raw.extend_from_slice(term.as_bytes());
            write_varint(*wtf as u64, &mut raw);
            write_varint(fpos.len() as u64, &mut raw);
            for (fi, positions) in fpos {
                raw.push(*fi);
                write_varint(positions.len() as u64, &mut raw);
                let mut prev = 0u32;
                for &p in positions {
                    write_varint((p - prev) as u64, &mut raw);
                    prev = p;
                }
            }
        }
        if raw.len() > MAX_DOC_BYTES {
            return Err(ElmdbError::new(
                ErrorCode::ResourceLimit,
                format!("encoded text doc {} B exceeds {} B cap", raw.len(), MAX_DOC_BYTES),
            ));
        }
        if raw.len() >= DOC_ZSTD_THRESHOLD {
            let compressed =
                zstd::bulk::compress(&raw, 1).map_err(|e| ElmdbError::new(ErrorCode::Internal, e.to_string()))?;
            if compressed.len() + 5 < raw.len() {
                let mut out = Vec::with_capacity(5 + compressed.len());
                out.push(DOC_ZSTD);
                out.extend_from_slice(&(raw.len() as u32).to_le_bytes());
                out.extend_from_slice(&compressed);
                return Ok(out);
            }
        }
        let mut out = Vec::with_capacity(1 + raw.len());
        out.push(DOC_RAW);
        out.extend_from_slice(&raw);
        Ok(out)
    }

    fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.is_empty() {
            return Err(ElmdbError::new(ErrorCode::Corrupted, "empty stored text doc"));
        }
        let owned;
        let raw: &[u8] = match bytes[0] {
            DOC_RAW => &bytes[1..],
            DOC_ZSTD => {
                if bytes.len() < 5 {
                    return Err(ElmdbError::new(ErrorCode::Corrupted, "truncated zstd text doc"));
                }
                let expected = u32::from_le_bytes(bytes[1..5].try_into().expect("4")) as usize;
                if expected > MAX_DOC_BYTES {
                    return Err(ElmdbError::new(ErrorCode::Corrupted, "text doc exceeds 32 MiB cap"));
                }
                let decoder = zstd::stream::read::Decoder::new(&bytes[5..])
                    .map_err(|e| ElmdbError::new(ErrorCode::Corrupted, e.to_string()))?;
                let mut buf = Vec::with_capacity(expected.min(64 * 1024));
                decoder
                    .take((expected as u64) + 1)
                    .read_to_end(&mut buf)
                    .map_err(|e| ElmdbError::new(ErrorCode::Corrupted, e.to_string()))?;
                if buf.len() != expected {
                    return Err(ElmdbError::new(ErrorCode::Corrupted, "text doc decompressed length mismatch"));
                }
                owned = buf;
                &owned
            }
            other => return Err(ElmdbError::new(ErrorCode::Corrupted, format!("unknown doc flag {other}"))),
        };

        let mut pos = 0usize;
        let weighted_doc_len = read_varint(raw, &mut pos)?;
        let fl_count = read_varint(raw, &mut pos)? as usize;
        let mut field_lens = Vec::with_capacity(fl_count.min(64));
        for _ in 0..fl_count {
            field_lens.push(read_varint(raw, &mut pos)?);
        }
        let term_count = read_varint(raw, &mut pos)? as usize;
        let mut terms = Vec::with_capacity(term_count.min(1024));
        for _ in 0..term_count {
            let tlen = read_varint(raw, &mut pos)? as usize;
            let end = pos
                .checked_add(tlen)
                .ok_or_else(|| ElmdbError::new(ErrorCode::Corrupted, "term length overflow in stored doc"))?;
            if end > raw.len() {
                return Err(ElmdbError::new(ErrorCode::Corrupted, "truncated term in stored doc"));
            }
            let term = std::str::from_utf8(&raw[pos..end])
                .map_err(|_| ElmdbError::new(ErrorCode::Corrupted, "non-utf8 term"))?
                .to_owned();
            pos = end;
            let wtf = read_varint(raw, &mut pos)? as u32;
            let fcount = read_varint(raw, &mut pos)? as usize;
            let mut fpos = Vec::with_capacity(fcount.min(32));
            for _ in 0..fcount {
                if pos >= raw.len() {
                    return Err(ElmdbError::new(ErrorCode::Corrupted, "truncated field idx"));
                }
                let fi = raw[pos];
                pos += 1;
                let pcount = read_varint(raw, &mut pos)? as usize;
                let mut positions = Vec::with_capacity(pcount.min(256));
                let mut cur = 0u32;
                for _ in 0..pcount {
                    cur = cur.saturating_add(read_varint(raw, &mut pos)? as u32);
                    positions.push(cur);
                }
                fpos.push((fi, positions));
            }
            terms.push((term, wtf, fpos));
        }
        if pos != raw.len() {
            return Err(ElmdbError::new(ErrorCode::Corrupted, "trailing bytes in stored doc"));
        }
        Ok(Self { weighted_doc_len, field_lens, terms })
    }
}

fn write_varint(mut v: u64, out: &mut Vec<u8>) {
    while v >= 0x80 {
        out.push(((v as u8) & 0x7F) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

fn read_varint(raw: &[u8], pos: &mut usize) -> Result<u64> {
    let mut out = 0u64;
    let mut shift = 0u32;
    loop {
        if *pos >= raw.len() || shift >= 64 {
            return Err(ElmdbError::new(ErrorCode::Corrupted, "corrupt varint"));
        }
        let b = raw[*pos];
        *pos += 1;
        out |= ((b & 0x7F) as u64) << shift;
        if (b & 0x80) == 0 {
            return Ok(out);
        }
        shift += 7;
    }
}

fn dict_key(term: &str) -> Vec<u8> {
    let mut k = Vec::with_capacity(1 + term.len());
    k.push(TAG_DICT);
    k.extend_from_slice(term.as_bytes());
    k
}

fn block_key(term: &str, bucket: u64) -> Vec<u8> {
    let mut k = Vec::with_capacity(1 + term.len() + 1 + 8);
    k.push(TAG_BLOCK);
    k.extend_from_slice(term.as_bytes());
    k.push(0x00);
    k.extend_from_slice(&bucket.to_be_bytes());
    k
}

fn doc_key(key: EntityKey) -> [u8; 9] {
    let mut k = [0u8; 9];
    k[0] = TAG_DOCS;
    k[1..9].copy_from_slice(&key.to_be_bytes());
    k
}

fn length_key(bucket: u64) -> [u8; 9] {
    let mut k = [0u8; 9];
    k[0] = TAG_LENGTHS;
    k[1..9].copy_from_slice(&bucket.to_be_bytes());
    k
}

fn encode_length_block(entries: &[(u8, u64)]) -> Vec<u8> {
    let mut out = Vec::with_capacity(2 + entries.len() * 9);
    out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    for &(off, len) in entries {
        out.push(off);
        out.extend_from_slice(&len.to_le_bytes());
    }
    out
}

fn decode_length_block(raw: &[u8]) -> Result<Vec<(u8, u64)>> {
    if raw.len() < 2 {
        return Err(ElmdbError::new(ErrorCode::Corrupted, "truncated length block"));
    }
    let n = u16::from_le_bytes([raw[0], raw[1]]) as usize;
    if raw.len() != 2 + n * 9 {
        return Err(ElmdbError::new(ErrorCode::Corrupted, "corrupt length block"));
    }
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let off = 2 + i * 9;
        let k_off = raw[off];
        let len = u64::from_le_bytes(raw[off + 1..off + 9].try_into().expect("8"));
        out.push((k_off, len));
    }
    Ok(out)
}

fn build_doc_from_row(
    row: &PackedRow,
    fields: &[(FieldId, u16)],
    stem_lang: Option<&str>,
) -> Result<Option<StoredDoc>> {
    if fields.len() > 256 {
        return Err(ElmdbError::new(ErrorCode::InvalidSchema, "text index supports at most 256 fields"));
    }
    let mut field_lens = vec![0u64; fields.len()];
    let mut weighted_doc_len = 0u64;
    // term -> (weighted_tf, BTreeMap<field_idx, Vec<pos>>)
    let mut by_term: BTreeMap<String, (u32, BTreeMap<u8, Vec<u32>>)> = BTreeMap::new();

    for (fi, &(fid, weight)) in fields.iter().enumerate() {
        let Some(val) = row.fields.get(&fid) else { continue };
        let w = weight.max(1) as u64;
        let mut texts: Vec<&str> = Vec::new();
        match val {
            CanonicalValue::String(s) => texts.push(s.as_str()),
            CanonicalValue::Array(items) => {
                for item in items {
                    if let CanonicalValue::String(s) = item {
                        texts.push(s.as_str());
                    }
                }
            }
            _ => continue,
        }
        let mut base_pos = 0u32;
        let mut word_count = 0u64;
        for text in texts {
            let tokens = analyze_text(text, stem_lang);
            let mut max_pos = 0u32;
            let mut had_token = false;
            for tok in tokens {
                had_token = true;
                max_pos = max_pos.max(tok.position);
                let abs_pos = base_pos.saturating_add(tok.position);
                let entry = by_term.entry(tok.term).or_insert_with(|| (0, BTreeMap::new()));
                entry.0 = entry.0.saturating_add(w as u32);
                entry.1.entry(fi as u8).or_default().push(abs_pos);
            }
            if had_token {
                let words_here = (max_pos as u64) + 1;
                word_count += words_here;
                base_pos = base_pos.saturating_add(max_pos + 1 + ELEMENT_GAP);
            }
        }
        field_lens[fi] = word_count;
        weighted_doc_len = weighted_doc_len.saturating_add(word_count.saturating_mul(w));
    }

    if by_term.is_empty() {
        return Ok(None);
    }

    let terms = by_term.into_iter().map(|(term, (wtf, fmap))| (term, wtf, fmap.into_iter().collect())).collect();
    Ok(Some(StoredDoc { weighted_doc_len: weighted_doc_len.max(1), field_lens, terms }))
}

fn recompute_dict_bounds_from_blocks<R: KvRead>(txn: &R, db: SubDbHandle, term: &str) -> Result<(u32, u64)> {
    let mut prefix = Vec::with_capacity(2 + term.len());
    prefix.push(TAG_BLOCK);
    prefix.extend_from_slice(term.as_bytes());
    prefix.push(0x00);
    let mut cur = txn.cursor(db)?;
    let mut max_tf = 0u32;
    let mut min_len = u64::MAX;
    let mut step = cur.seek_ge(&prefix)?;
    while let Some((k, v)) = step {
        if k.len() != prefix.len() + 8 || !k.starts_with(&prefix) {
            break;
        }
        let (btf, blen, _) = PostingBlock::decode_header(v)?;
        max_tf = max_tf.max(btf);
        min_len = min_len.min(blen);
        step = cur.next()?;
    }
    Ok((max_tf, min_len))
}

fn remove_stored_doc<W: KvMut>(
    txn: &mut W,
    db: SubDbHandle,
    key: EntityKey,
    old_doc: &StoredDoc,
    stats: &mut TextStats,
) -> Result<()> {
    let bucket = key.0 >> BLOCK_SHIFT;
    let offset = (key.0 & ((1u64 << BLOCK_SHIFT) - 1)) as u8;

    stats.doc_count = stats.doc_count.saturating_sub(1);
    stats.total_weighted_len = stats.total_weighted_len.saturating_sub(old_doc.weighted_doc_len);
    for (s, &fl) in stats.field_len_totals.iter_mut().zip(old_doc.field_lens.iter()) {
        *s = s.saturating_sub(fl);
    }

    // Remove length entry.
    let lk = length_key(bucket);
    if let Some(raw) = txn.get(db, &lk)?.map(ToOwned::to_owned) {
        let mut entries = decode_length_block(&raw)?;
        if let Ok(idx) = entries.binary_search_by_key(&offset, |&(o, _)| o) {
            entries.remove(idx);
            if entries.is_empty() {
                txn.del(db, &lk)?;
            } else {
                txn.put(db, &lk, &encode_length_block(&entries))?;
            }
        }
    }

    // Remove postings & update dict.
    for (term, old_wtf, _) in &old_doc.terms {
        let bk = block_key(term, bucket);
        if let Some(raw) = txn.get(db, &bk)?.map(ToOwned::to_owned) {
            let mut blk = PostingBlock::decode(&raw)?;
            if let Ok(idx) = blk.postings.binary_search_by_key(&offset, |p| p.offset) {
                blk.postings.remove(idx);
                if blk.postings.is_empty() {
                    txn.del(db, &bk)?;
                } else {
                    blk.recompute_bounds();
                    txn.put(db, &bk, &blk.encode())?;
                }
            }
        }
        let dk = dict_key(term);
        if let Some(raw) = txn.get(db, &dk)?.map(ToOwned::to_owned) {
            let mut entry = TermDictEntry::decode(&raw)?;
            entry.doc_freq = entry.doc_freq.saturating_sub(1);
            if entry.doc_freq == 0 {
                txn.del(db, &dk)?;
            } else {
                if *old_wtf >= entry.max_weighted_tf || old_doc.weighted_doc_len <= entry.min_weighted_doc_len {
                    let (mtf, mlen) = recompute_dict_bounds_from_blocks(txn, db, term)?;
                    entry.max_weighted_tf = mtf;
                    entry.min_weighted_doc_len = mlen;
                }
                txn.put(db, &dk, &entry.encode())?;
            }
        }
    }

    txn.del(db, &doc_key(key))?;
    Ok(())
}

fn insert_stored_doc<W: KvMut>(
    txn: &mut W,
    db: SubDbHandle,
    key: EntityKey,
    doc: &StoredDoc,
    stats: &mut TextStats,
) -> Result<()> {
    let bucket = key.0 >> BLOCK_SHIFT;
    let offset = (key.0 & ((1u64 << BLOCK_SHIFT) - 1)) as u8;

    stats.doc_count = stats.doc_count.saturating_add(1);
    stats.total_weighted_len = stats.total_weighted_len.saturating_add(doc.weighted_doc_len);
    for (s, &fl) in stats.field_len_totals.iter_mut().zip(doc.field_lens.iter()) {
        *s = s.saturating_add(fl);
    }

    // Insert length entry.
    let lk = length_key(bucket);
    let mut entries = match txn.get(db, &lk)?.map(ToOwned::to_owned) {
        Some(raw) => decode_length_block(&raw)?,
        None => Vec::new(),
    };
    match entries.binary_search_by_key(&offset, |&(o, _)| o) {
        Ok(idx) => entries[idx].1 = doc.weighted_doc_len,
        Err(idx) => entries.insert(idx, (offset, doc.weighted_doc_len)),
    }
    txn.put(db, &lk, &encode_length_block(&entries))?;

    // Insert postings & update dict.
    for (term, wtf, _) in &doc.terms {
        let bk = block_key(term, bucket);
        let mut blk = match txn.get(db, &bk)?.map(ToOwned::to_owned) {
            Some(raw) => PostingBlock::decode(&raw)?,
            None => PostingBlock { max_weighted_tf: 0, min_weighted_doc_len: u64::MAX, postings: Vec::new() },
        };
        let posting = Posting { offset, weighted_tf: *wtf, weighted_doc_len: doc.weighted_doc_len };
        match blk.postings.binary_search_by_key(&offset, |p| p.offset) {
            Ok(idx) => blk.postings[idx] = posting,
            Err(idx) => blk.postings.insert(idx, posting),
        }
        blk.recompute_bounds();
        txn.put(db, &bk, &blk.encode())?;

        let dk = dict_key(term);
        let mut entry = match txn.get(db, &dk)?.map(ToOwned::to_owned) {
            Some(raw) => TermDictEntry::decode(&raw)?,
            None => TermDictEntry { doc_freq: 0, max_weighted_tf: 0, min_weighted_doc_len: u64::MAX },
        };
        entry.doc_freq = entry.doc_freq.saturating_add(1);
        entry.max_weighted_tf = entry.max_weighted_tf.max(*wtf);
        entry.min_weighted_doc_len = entry.min_weighted_doc_len.min(doc.weighted_doc_len);
        txn.put(db, &dk, &entry.encode())?;
    }

    txn.put(db, &doc_key(key), &doc.encode()?)?;
    Ok(())
}

/// Applies an incremental live-row `(before, after)` delta to a BM25F text index.
pub fn apply_delta<W: KvMut>(
    txn: &mut W,
    db: SubDbHandle,
    fields: &[(FieldId, u16)],
    stem_lang: Option<&str>,
    key: EntityKey,
    _before: Option<&PackedRow>,
    after: Option<&PackedRow>,
) -> Result<()> {
    let mut stats = match txn.get(db, &[TAG_STATS])?.map(ToOwned::to_owned) {
        Some(raw) => TextStats::decode(&raw)?,
        None => TextStats::empty(fields.len()),
    };

    let existing_doc = txn.get(db, &doc_key(key))?.map(ToOwned::to_owned);
    let new_doc = match after {
        Some(r) => build_doc_from_row(r, fields, stem_lang)?,
        None => None,
    };

    if let Some(old_raw) = existing_doc {
        let old_doc = StoredDoc::decode(&old_raw)?;
        if new_doc.as_ref() == Some(&old_doc) {
            return Ok(());
        }
        remove_stored_doc(txn, db, key, &old_doc, &mut stats)?;
    }

    if let Some(doc) = new_doc {
        insert_stored_doc(txn, db, key, &doc, &mut stats)?;
    }

    if stats.doc_count == 0 {
        txn.del(db, &[TAG_STATS])?;
    } else {
        txn.put(db, &[TAG_STATS], &stats.encode())?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// BM25F + Block-Max WAND query primitives (consumed by `elmdb-query`)
// ---------------------------------------------------------------------------

#[inline]
pub fn bm25_idf(doc_count: u64, doc_freq: u32) -> f64 {
    let n = doc_count as f64;
    let df = doc_freq as f64;
    (((n - df + 0.5) / (df + 0.5)) + 1.0).ln().max(0.0)
}

#[inline]
pub fn bm25_term_score(idf: f64, weighted_tf: u32, weighted_doc_len: u64, avgdl: f64) -> f64 {
    let tf = weighted_tf as f64;
    let dl = weighted_doc_len as f64;
    let norm = (1.0 - BM25_B) + BM25_B * (dl / avgdl.max(1.0));
    idf * (tf * (BM25_K1 + 1.0)) / (tf + BM25_K1 * norm)
}

/// O(matched_dict_keys) prefix expansion reading ONLY `TAG_DICT` (zero posting block reads!).
/// Returns up to `limit` terms ordered by descending `doc_freq`, breaking ties lexicographically.
pub fn prefix_terms<R: KvRead>(
    txn: &R,
    db: SubDbHandle,
    prefix: &str,
    limit: usize,
) -> Result<Vec<(String, TermDictEntry)>> {
    if prefix.is_empty() || limit == 0 {
        return Ok(Vec::new());
    }
    let key_prefix = dict_key(prefix);
    let mut cur = txn.cursor(db)?;
    let mut matches = Vec::new();
    let mut step = cur.seek_ge(&key_prefix)?;
    while let Some((k, v)) = step {
        if !k.starts_with(&key_prefix) {
            break;
        }
        let term = std::str::from_utf8(&k[1..])
            .map_err(|_| ElmdbError::new(ErrorCode::Corrupted, "non-utf8 dict key"))?
            .to_owned();
        let entry = TermDictEntry::decode(v)?;
        matches.push((term, entry));
        step = cur.next()?;
    }
    matches.sort_by(|a, b| b.1.doc_freq.cmp(&a.1.doc_freq).then_with(|| a.0.cmp(&b.0)));
    matches.truncate(limit);
    Ok(matches)
}

/// Analyzes a sequence of phrase elements into `(term, relative_position)` pairs, preserving
/// co-located compound code tokens (e.g. `THR-12` emits `"thr"` and `"thr-12"` at `rel_pos = 0`
/// and `"12"` at `rel_pos = 1`, followed by the next element at `rel_pos = 2`) and preserving
/// oversized (`> MAX_TERM_BYTES`) words so phrases containing or ending on an unindexed long
/// word never silently truncate and falsely match shorter prefixes.
pub fn analyze_phrase(terms: &[String], stem_lang: Option<&str>) -> Vec<(String, u32)> {
    let stemmer = stem_lang.and_then(stem_algorithm).map(Stemmer::create);
    let mut out = Vec::new();
    let mut base_pos = 0u32;
    for t in terms {
        let mut words: Vec<(String, u32, usize, usize)> = Vec::new();
        for token in t.as_str().tokenize() {
            if !token.is_word() || token.lemma().is_empty() {
                continue;
            }
            words.push((token.lemma().to_owned(), words.len() as u32, token.byte_start, token.byte_end));
        }
        if words.is_empty() {
            let lower = t.to_lowercase();
            if !lower.is_empty() {
                out.push((lower, base_pos));
                base_pos = base_pos.saturating_add(1);
            }
        } else {
            for (idx, (lemma, pos, _byte_start, byte_end)) in words.iter().enumerate() {
                let stemmed = match &stemmer {
                    Some(s) => s.stem(lemma).into_owned(),
                    None => lemma.clone(),
                };
                out.push((stemmed, base_pos.saturating_add(*pos)));
                if let Some((next_lemma, _, next_start, _)) = words.get(idx + 1)
                    && t.get(*byte_end..*next_start) == Some("-")
                    && lemma.chars().all(char::is_alphabetic)
                    && next_lemma.chars().all(|c| c.is_ascii_digit())
                {
                    let code = format!("{lemma}-{next_lemma}");
                    if code.len() <= MAX_TERM_BYTES {
                        out.push((code, base_pos.saturating_add(*pos)));
                    }
                }
            }
            base_pos = base_pos.saturating_add(words.len() as u32);
        }
    }
    out
}

/// Checks whether document `key` contains `phrase_terms` consecutively within the same field.
pub fn matches_phrase<R: KvRead>(txn: &R, db: SubDbHandle, key: EntityKey, phrase_terms: &[String]) -> Result<bool> {
    let positioned: Vec<(String, u32)> = phrase_terms.iter().enumerate().map(|(i, t)| (t.clone(), i as u32)).collect();
    matches_phrase_positions(txn, db, key, &positioned)
}

/// Checks whether document `key` contains `(term, relative_position)` phrase terms within the same field.
pub fn matches_phrase_positions<R: KvRead>(
    txn: &R,
    db: SubDbHandle,
    key: EntityKey,
    phrase_terms: &[(String, u32)],
) -> Result<bool> {
    if phrase_terms.is_empty() {
        return Ok(true);
    }
    let Some(raw) = txn.get(db, &doc_key(key))? else {
        return Ok(false);
    };
    let doc = StoredDoc::decode(raw)?;
    if phrase_terms.len() == 1 {
        return Ok(doc.terms.binary_search_by(|(t, _, _)| t.as_str().cmp(phrase_terms[0].0.as_str())).is_ok());
    }
    // Map each phrase term to its `(rel_pos, per-field position lists)`.
    type FieldPosSlice<'a> = &'a [(u8, Vec<u32>)];
    let mut lists: Vec<(u32, FieldPosSlice<'_>)> = Vec::with_capacity(phrase_terms.len());
    for (pt, rel_pos) in phrase_terms {
        let Ok(idx) = doc.terms.binary_search_by(|(t, _, _)| t.as_str().cmp(pt.as_str())) else {
            return Ok(false);
        };
        lists.push((*rel_pos, &doc.terms[idx].2));
    }
    let first_rel = lists[0].0;
    // Check each field present in the first term.
    for (fi, first_positions) in lists[0].1 {
        let mut field_pos_lists: Vec<(u32, &[u32])> = Vec::with_capacity(lists.len());
        field_pos_lists.push((first_rel, first_positions.as_slice()));
        let mut all_in_field = true;
        for &(rel_pos, other) in &lists[1..] {
            if let Some((_, ps)) = other.iter().find(|(f, _)| f == fi) {
                field_pos_lists.push((rel_pos, ps.as_slice()));
            } else {
                all_in_field = false;
                break;
            }
        }
        if !all_in_field {
            continue;
        }
        for &start_pos in field_pos_lists[0].1 {
            let Some(anchor) = start_pos.checked_sub(first_rel) else {
                continue;
            };
            let mut matched = true;
            for &(rel_pos, ps) in &field_pos_lists[1..] {
                let target = anchor.saturating_add(rel_pos);
                if ps.binary_search(&target).is_err() {
                    matched = false;
                    break;
                }
            }
            if matched {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

/// Scored BM25F search hit.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TextHit {
    pub key: EntityKey,
    pub score: f64,
}

#[derive(Debug, Clone, Copy)]
struct HeapHit {
    score: f64,
    key: EntityKey,
}

impl PartialEq for HeapHit {
    fn eq(&self, other: &Self) -> bool {
        self.key == other.key && self.score.to_bits() == other.score.to_bits()
    }
}
impl Eq for HeapHit {}

impl PartialOrd for HeapHit {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Min-heap ordering: smallest score at top; on score tie, LARGEST key at top so smaller keys win.
impl Ord for HeapHit {
    fn cmp(&self, other: &Self) -> Ordering {
        other.score.total_cmp(&self.score).then_with(|| self.key.cmp(&other.key))
    }
}

const MAX_VALID_BUCKET: u64 = 1u64 << (64 - BLOCK_SHIFT);
/// Maximum number of dictionary terms expanded per query token when `prefix: true`.
pub const MAX_PREFIX_EXPANSIONS: usize = 64;

/// Block-Max WAND cursor over a single term's posting blocks.
struct BmwTermCursor {
    term: String,
    idf: f64,
    term_ub: f64,
    /// `(bucket, max_weighted_tf, min_weighted_doc_len)` — raw block payloads are fetched lazily from `txn`!
    blocks: Vec<(u64, u32, u64)>,
    block_idx: usize,
    decoded_block_idx: Option<usize>,
    decoded: Vec<Posting>,
    post_idx: usize,
    cur_doc: Option<EntityKey>,
    cur_posting: Option<Posting>,
    /// Count of posting blocks skipped by BMW upper-bound pruning (telemetry for tests/EXPLAIN).
    blocks_pruned: usize,
}

impl BmwTermCursor {
    fn open<R: KvRead>(txn: &R, db: SubDbHandle, term: &str, stats: &TextStats) -> Result<Option<Self>> {
        let Some(dict_raw) = txn.get(db, &dict_key(term))? else {
            return Ok(None);
        };
        let dict = TermDictEntry::decode(dict_raw)?;
        if dict.doc_freq == 0 {
            return Ok(None);
        }
        let avgdl = (stats.total_weighted_len as f64 / stats.doc_count.max(1) as f64).max(1.0);
        let idf = bm25_idf(stats.doc_count, dict.doc_freq);
        let term_ub = bm25_term_score(idf, dict.max_weighted_tf, dict.min_weighted_doc_len, avgdl);

        let mut prefix = Vec::with_capacity(2 + term.len());
        prefix.push(TAG_BLOCK);
        prefix.extend_from_slice(term.as_bytes());
        prefix.push(0x00);

        let mut blocks = Vec::new();
        {
            let mut cur = txn.cursor(db)?;
            let mut step = cur.seek_ge(&prefix)?;
            while let Some((k, v)) = step {
                if k.len() != prefix.len() + 8 || !k.starts_with(&prefix) {
                    break;
                }
                let bucket = u64::from_be_bytes(k[prefix.len()..prefix.len() + 8].try_into().expect("8"));
                if bucket >= MAX_VALID_BUCKET {
                    return Err(ElmdbError::new(ErrorCode::Corrupted, "posting block bucket out of u64 key range"));
                }
                let (max_tf, min_len, _) = PostingBlock::decode_header(v)?;
                blocks.push((bucket, max_tf, min_len));
                step = cur.next()?;
            }
        }
        if blocks.is_empty() {
            return Ok(None);
        }
        let mut c = Self {
            term: term.to_owned(),
            idf,
            term_ub,
            blocks,
            block_idx: 0,
            decoded_block_idx: None,
            decoded: Vec::new(),
            post_idx: 0,
            cur_doc: None,
            cur_posting: None,
            blocks_pruned: 0,
        };
        c.advance_to(txn, db, EntityKey(0))?;
        Ok(Some(c))
    }

    fn exhaust(&mut self) {
        if self.block_idx < self.blocks.len() {
            let remaining =
                self.blocks.len() - self.block_idx - usize::from(self.decoded_block_idx == Some(self.block_idx));
            self.blocks_pruned += remaining;
        }
        self.block_idx = self.blocks.len();
        self.decoded_block_idx = None;
        self.cur_doc = None;
        self.cur_posting = None;
    }

    fn block_ub_at(&self, target: EntityKey, avgdl: f64) -> f64 {
        let target_bucket = target.0 >> BLOCK_SHIFT;
        let idx = self.blocks[self.block_idx..].partition_point(|(b, _, _)| *b < target_bucket) + self.block_idx;
        if idx >= self.blocks.len() || self.blocks[idx].0 != target_bucket {
            return 0.0;
        }
        let (_, max_tf, min_len) = self.blocks[idx];
        bm25_term_score(self.idf, max_tf, min_len, avgdl)
    }

    fn block_end_covering(&self, target: EntityKey) -> u64 {
        let target_bucket = target.0 >> BLOCK_SHIFT;
        let idx = self.blocks[self.block_idx..].partition_point(|(b, _, _)| *b < target_bucket) + self.block_idx;
        if idx >= self.blocks.len() {
            return u64::MAX;
        }
        let b = self.blocks[idx].0;
        if b == target_bucket {
            (b << BLOCK_SHIFT) | ((1u64 << BLOCK_SHIFT) - 1)
        } else {
            // Next block starts strictly after target_bucket; safe to jump to start of that block - 1.
            (b << BLOCK_SHIFT).saturating_sub(1)
        }
    }

    /// Advances cursor to the smallest `EntityKey >= target`, skipping entire blocks without decoding or reading them.
    fn advance_to<R: KvRead>(&mut self, txn: &R, db: SubDbHandle, target: EntityKey) -> Result<()> {
        if let Some(cur) = self.cur_doc
            && cur >= target
        {
            return Ok(());
        }
        let target_bucket = target.0 >> BLOCK_SHIFT;
        if self.block_idx < self.blocks.len() && self.blocks[self.block_idx].0 < target_bucket {
            let new_idx =
                self.blocks[self.block_idx..].partition_point(|(b, _, _)| *b < target_bucket) + self.block_idx;
            if new_idx > self.block_idx {
                let skipped = new_idx - self.block_idx - usize::from(self.decoded_block_idx == Some(self.block_idx));
                self.blocks_pruned += skipped;
            }
            self.block_idx = new_idx;
            self.decoded_block_idx = None;
            self.post_idx = 0;
        }

        while self.block_idx < self.blocks.len() {
            let bucket = self.blocks[self.block_idx].0;
            if self.decoded_block_idx != Some(self.block_idx) {
                let raw = txn
                    .get(db, &block_key(&self.term, bucket))?
                    .ok_or_else(|| ElmdbError::new(ErrorCode::Corrupted, "missing posting block during BMW scan"))?;
                let blk = PostingBlock::decode(raw)?;
                self.decoded = blk.postings;
                self.decoded_block_idx = Some(self.block_idx);
                self.post_idx = 0;
            }
            let base = bucket << BLOCK_SHIFT;
            while self.post_idx < self.decoded.len() {
                let p = self.decoded[self.post_idx];
                let ek = EntityKey(base | (p.offset as u64));
                if ek >= target {
                    self.cur_doc = Some(ek);
                    self.cur_posting = Some(p);
                    return Ok(());
                }
                self.post_idx += 1;
            }
            self.block_idx += 1;
            self.decoded_block_idx = None;
            self.post_idx = 0;
        }
        self.cur_doc = None;
        self.cur_posting = None;
        Ok(())
    }
}

/// Disjunction-max BMW cursor for a single prefix-expanded query term (up to `MAX_PREFIX_EXPANSIONS` dict terms).
///
/// Preserves exact Block-Max WAND upper-bound soundness:
/// - `term_ub = max_i child_i.term_ub`
/// - `block_ub_at(target) = max_i child_i.block_ub_at(target)`
/// - `block_end_covering(target) = min_i child_i.block_end_covering(target)`
/// - `score_at(doc) = max_{child_i at doc} bm25_term_score(child_i)`
struct PrefixGroupCursor {
    children: Vec<BmwTermCursor>,
    term_ub: f64,
    cur_doc: Option<EntityKey>,
    blocks_pruned: usize,
}

impl PrefixGroupCursor {
    fn open<R: KvRead>(txn: &R, db: SubDbHandle, prefix: &str, stats: &TextStats) -> Result<Option<Self>> {
        let mut expanded = prefix_terms(txn, db, prefix, MAX_PREFIX_EXPANSIONS)?;
        // Guarantee the exact term itself is never dropped by top-N truncation.
        if !expanded.iter().any(|(t, _)| t == prefix)
            && let Some(raw) = txn.get(db, &dict_key(prefix))?
        {
            let entry = TermDictEntry::decode(raw)?;
            if entry.doc_freq > 0 {
                expanded.push((prefix.to_owned(), entry));
            }
        }
        if expanded.is_empty() {
            return Ok(None);
        }
        let mut children = Vec::with_capacity(expanded.len());
        let mut term_ub = 0.0f64;
        for (term, _) in expanded {
            if let Some(c) = BmwTermCursor::open(txn, db, &term, stats)? {
                term_ub = term_ub.max(c.term_ub);
                children.push(c);
            }
        }
        if children.is_empty() {
            return Ok(None);
        }
        let cur_doc = children.iter().filter_map(|c| c.cur_doc).min();
        Ok(Some(Self { children, term_ub, cur_doc, blocks_pruned: 0 }))
    }

    fn exhaust(&mut self) {
        for c in &mut self.children {
            c.exhaust();
            self.blocks_pruned += c.blocks_pruned;
            c.blocks_pruned = 0;
        }
        self.children.clear();
        self.cur_doc = None;
    }

    fn block_ub_at(&self, target: EntityKey, avgdl: f64) -> f64 {
        self.children.iter().map(|c| c.block_ub_at(target, avgdl)).fold(0.0f64, f64::max)
    }

    fn block_end_covering(&self, target: EntityKey) -> u64 {
        self.children.iter().map(|c| c.block_end_covering(target)).min().unwrap_or(u64::MAX)
    }

    fn score_at(&self, doc: EntityKey, avgdl: f64) -> f64 {
        let mut best = 0.0f64;
        for c in &self.children {
            if c.cur_doc == Some(doc)
                && let Some(p) = c.cur_posting
            {
                best = best.max(bm25_term_score(c.idf, p.weighted_tf, p.weighted_doc_len, avgdl));
            }
        }
        best
    }

    fn advance_to<R: KvRead>(&mut self, txn: &R, db: SubDbHandle, target: EntityKey) -> Result<()> {
        if let Some(cur) = self.cur_doc
            && cur >= target
        {
            return Ok(());
        }
        let mut i = 0;
        let mut min_doc: Option<EntityKey> = None;
        let mut removed_any = false;
        while i < self.children.len() {
            if self.children[i].cur_doc.is_some_and(|d| d < target) {
                self.children[i].advance_to(txn, db, target)?;
            }
            if let Some(d) = self.children[i].cur_doc {
                min_doc = Some(match min_doc {
                    Some(prev) => prev.min(d),
                    None => d,
                });
                i += 1;
            } else {
                self.blocks_pruned += self.children[i].blocks_pruned;
                self.children.swap_remove(i);
                removed_any = true;
            }
        }
        if removed_any {
            self.term_ub = self.children.iter().map(|c| c.term_ub).fold(0.0f64, f64::max);
        }
        self.cur_doc = min_doc;
        Ok(())
    }

    fn total_blocks_pruned(&self) -> usize {
        self.blocks_pruned + self.children.iter().map(|c| c.blocks_pruned).sum::<usize>()
    }
}

/// Unified Block-Max WAND query term cursor (either exact single term or prefix-expanded disjunction-max group).
enum QueryTermCursor {
    Single(BmwTermCursor),
    Prefix(PrefixGroupCursor),
}

impl QueryTermCursor {
    fn cur_doc(&self) -> Option<EntityKey> {
        match self {
            Self::Single(c) => c.cur_doc,
            Self::Prefix(g) => g.cur_doc,
        }
    }

    fn term_ub(&self) -> f64 {
        match self {
            Self::Single(c) => c.term_ub,
            Self::Prefix(g) => g.term_ub,
        }
    }

    fn exhaust(&mut self) {
        match self {
            Self::Single(c) => c.exhaust(),
            Self::Prefix(g) => g.exhaust(),
        }
    }

    fn block_ub_at(&self, target: EntityKey, avgdl: f64) -> f64 {
        match self {
            Self::Single(c) => c.block_ub_at(target, avgdl),
            Self::Prefix(g) => g.block_ub_at(target, avgdl),
        }
    }

    fn block_end_covering(&self, target: EntityKey) -> u64 {
        match self {
            Self::Single(c) => c.block_end_covering(target),
            Self::Prefix(g) => g.block_end_covering(target),
        }
    }

    fn score_at(&self, doc: EntityKey, avgdl: f64) -> f64 {
        match self {
            Self::Single(c) => {
                if c.cur_doc == Some(doc)
                    && let Some(p) = c.cur_posting
                {
                    bm25_term_score(c.idf, p.weighted_tf, p.weighted_doc_len, avgdl)
                } else {
                    0.0
                }
            }
            Self::Prefix(g) => g.score_at(doc, avgdl),
        }
    }

    fn advance_to<R: KvRead>(&mut self, txn: &R, db: SubDbHandle, target: EntityKey) -> Result<()> {
        match self {
            Self::Single(c) => c.advance_to(txn, db, target),
            Self::Prefix(g) => g.advance_to(txn, db, target),
        }
    }

    fn total_blocks_pruned(&self) -> usize {
        match self {
            Self::Single(c) => c.blocks_pruned,
            Self::Prefix(g) => g.total_blocks_pruned(),
        }
    }
}

/// Statistics returned alongside `search_bmw` hits (useful for `EXPLAIN` and verifying BMW pruning).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BmwSearchStats {
    pub docs_scored: usize,
    pub blocks_pruned: usize,
}

/// Executes top-`k` BM25F search using Block-Max WAND (exact top-`k` equivalence with brute-force BM25F!).
///
/// - `require_all`: when `true`, requires every query term to appear in the document (`AND`);
///   when `false`, scores documents containing any query term (`OR`).
/// - `candidates`: optional pushed-down candidate filter set.
pub fn search_bmw<R: KvRead>(
    txn: &R,
    db: SubDbHandle,
    query: &str,
    stem_lang: Option<&str>,
    top_k: usize,
    require_all: bool,
    candidates: Option<&FxHashSet<EntityKey>>,
) -> Result<(Vec<TextHit>, BmwSearchStats)> {
    search_bmw_with_prefix(txn, db, query, stem_lang, top_k, require_all, false, candidates)
}

/// Executes top-`k` BM25F search using Block-Max WAND with optional dictionary prefix expansion (`prefix: true`).
#[allow(clippy::too_many_arguments)]
pub fn search_bmw_with_prefix<R: KvRead>(
    txn: &R,
    db: SubDbHandle,
    query: &str,
    stem_lang: Option<&str>,
    top_k: usize,
    require_all: bool,
    prefix: bool,
    candidates: Option<&FxHashSet<EntityKey>>,
) -> Result<(Vec<TextHit>, BmwSearchStats)> {
    if top_k == 0 {
        return Ok((Vec::new(), BmwSearchStats::default()));
    }
    let Some(stats_raw) = txn.get(db, &[TAG_STATS])? else {
        return Ok((Vec::new(), BmwSearchStats::default()));
    };
    let stats = TextStats::decode(stats_raw)?;
    if stats.doc_count == 0 {
        return Ok((Vec::new(), BmwSearchStats::default()));
    }
    let avgdl = (stats.total_weighted_len as f64 / stats.doc_count as f64).max(1.0);

    let mut terms: Vec<String> = Vec::new();
    let mut seen: FxHashSet<String> = FxHashSet::default();
    for tok in analyze_text(query, stem_lang) {
        if seen.insert(tok.term.clone()) {
            terms.push(tok.term);
        }
    }
    if terms.is_empty() {
        return Ok((Vec::new(), BmwSearchStats::default()));
    }

    let mut cursors: Vec<QueryTermCursor> = Vec::with_capacity(terms.len());
    for t in &terms {
        let opened = if prefix {
            PrefixGroupCursor::open(txn, db, t, &stats)?.map(|mut g| {
                if g.children.len() == 1 {
                    QueryTermCursor::Single(g.children.pop().expect("len 1"))
                } else {
                    QueryTermCursor::Prefix(g)
                }
            })
        } else {
            BmwTermCursor::open(txn, db, t, &stats)?.map(QueryTermCursor::Single)
        };
        match opened {
            Some(c) => cursors.push(c),
            None => {
                if require_all {
                    return Ok((Vec::new(), BmwSearchStats::default()));
                }
            }
        }
    }
    if cursors.is_empty() {
        return Ok((Vec::new(), BmwSearchStats::default()));
    }

    let mut heap: BinaryHeap<HeapHit> = BinaryHeap::with_capacity(top_k.min(4096));
    let mut search_stats = BmwSearchStats::default();

    loop {
        // Drop exhausted cursors (accounting for their pruned blocks before dropping!).
        if require_all && cursors.iter().any(|c| c.cur_doc().is_none()) {
            break;
        }
        let mut i = 0;
        while i < cursors.len() {
            if cursors[i].cur_doc().is_none() {
                search_stats.blocks_pruned += cursors[i].total_blocks_pruned();
                cursors.swap_remove(i);
            } else {
                i += 1;
            }
        }
        if cursors.is_empty() {
            break;
        }

        // Sort active cursors by current doc ID ascending.
        cursors.sort_by_key(|c| c.cur_doc().unwrap());

        let threshold = if heap.len() == top_k { heap.peek().unwrap().score } else { 0.0 };
        let eps = 1e-12 * threshold.abs().max(1.0);

        // Pick pivot term.
        let mut pivot_idx = if require_all {
            let total_ub: f64 = cursors.iter().map(QueryTermCursor::term_ub).sum();
            if heap.len() == top_k && total_ub + eps < threshold {
                break;
            }
            cursors.len() - 1
        } else {
            let mut acc = 0.0;
            let mut found = None;
            for (idx, c) in cursors.iter().enumerate() {
                acc += c.term_ub();
                if heap.len() < top_k || acc + eps >= threshold {
                    found = Some(idx);
                    break;
                }
            }
            match found {
                Some(p) => p,
                None => break, // Remaining terms cannot beat top-K threshold!
            }
        };

        let pivot_doc = cursors[pivot_idx].cur_doc().unwrap();
        // Extend `pivot_idx` past any additional cursors tied at `pivot_doc` so `cursors[pivot_idx + 1]`
        // (if present) is strictly greater than `pivot_doc`.
        while pivot_idx + 1 < cursors.len() && cursors[pivot_idx + 1].cur_doc() == Some(pivot_doc) {
            pivot_idx += 1;
        }

        // Block-Max upper-bound check before scoring or aligning!
        if heap.len() == top_k {
            let mut block_sum = 0.0;
            for c in &cursors[..=pivot_idx] {
                block_sum += c.block_ub_at(pivot_doc, avgdl);
            }
            if block_sum + eps < threshold {
                // Skip safely past the shallowest block boundary covering `pivot_doc`, clamped by the
                // next cursor strictly after `pivot_doc` in OR mode so we never skip a doc where
                // trailing terms enter and combine to beat `threshold`!
                let mut next_target = u64::MAX;
                for c in &cursors[..=pivot_idx] {
                    next_target = next_target.min(c.block_end_covering(pivot_doc));
                }
                let next_after_block = next_target.checked_add(1);
                let next_after_pivot = pivot_doc.0.checked_add(1);
                let mut jump_opt = match (next_after_block, next_after_pivot) {
                    (Some(b), Some(p)) => Some(b.max(p)),
                    (None, Some(_)) => None, // Block reaches u64::MAX -> nothing left in block
                    (_, None) => None,       // pivot_doc == u64::MAX -> exhaust!
                };
                if !require_all && let Some(next_c) = cursors.get(pivot_idx + 1) {
                    let next_c_doc = next_c.cur_doc().unwrap().0;
                    jump_opt = Some(match jump_opt {
                        Some(j) => j.min(next_c_doc),
                        None => next_c_doc,
                    });
                }

                if require_all {
                    match jump_opt {
                        Some(j) => {
                            for c in &mut cursors {
                                c.advance_to(txn, db, EntityKey(j))?;
                            }
                        }
                        None => {
                            for c in &mut cursors {
                                c.exhaust();
                            }
                        }
                    }
                } else {
                    let target_cursor = cursors[..=pivot_idx]
                        .iter()
                        .enumerate()
                        .max_by(|(_, a), (_, b)| a.term_ub().total_cmp(&b.term_ub()))
                        .map_or(0, |(idx, _)| idx);
                    match jump_opt {
                        Some(j) => cursors[target_cursor].advance_to(txn, db, EntityKey(j))?,
                        None => cursors[target_cursor].exhaust(),
                    }
                }
                continue;
            }
        }

        if cursors[0].cur_doc() == Some(pivot_doc) {
            // All cursors in `0..=pivot_idx` are aligned on `pivot_doc`!
            let allowed = candidates.is_none_or(|set| set.contains(&pivot_doc));
            if allowed {
                let mut score = 0.0;
                for c in &cursors[..=pivot_idx] {
                    score += c.score_at(pivot_doc, avgdl);
                }
                search_stats.docs_scored += 1;
                let candidate = HeapHit { score, key: pivot_doc };
                if heap.len() < top_k {
                    heap.push(candidate);
                } else if candidate.cmp(heap.peek().unwrap()) == Ordering::Less {
                    heap.pop();
                    heap.push(candidate);
                }
            }
            match pivot_doc.0.checked_add(1) {
                Some(next_id) => {
                    let next_doc = EntityKey(next_id);
                    for c in &mut cursors[..=pivot_idx] {
                        c.advance_to(txn, db, next_doc)?;
                    }
                }
                None => {
                    for c in &mut cursors[..=pivot_idx] {
                        c.exhaust();
                    }
                }
            }
        } else {
            // Align preceding cursors up to `pivot_doc`.
            for c in &mut cursors[..pivot_idx] {
                if c.cur_doc().unwrap() < pivot_doc {
                    c.advance_to(txn, db, pivot_doc)?;
                }
            }
        }
    }

    for c in &mut cursors {
        c.exhaust();
        search_stats.blocks_pruned += c.total_blocks_pruned();
    }

    let mut results: Vec<TextHit> = heap.into_iter().map(|h| TextHit { key: h.key, score: h.score }).collect();
    results.sort_by(|a, b| b.score.total_cmp(&a.score).then_with(|| a.key.cmp(&b.key)));
    Ok((results, search_stats))
}

/// Computes lazy UTF-16 code-unit highlight spans `(element_index, utf16_start, utf16_end)`
/// per indexed text field for a single returned search hit.
///
/// Called ONLY on the final top-K hits — zero index storage overhead!
pub fn compute_highlights(
    row: &PackedRow,
    field_defs: &[(FieldId, &str)],
    stem_lang: Option<&str>,
    query: &str,
    phrase: Option<&[String]>,
    prefix: bool,
) -> BTreeMap<String, Vec<(u32, usize, usize)>> {
    let mut query_terms: Vec<String> = Vec::new();
    let mut seen: FxHashSet<String> = FxHashSet::default();
    for tok in analyze_text(query, stem_lang) {
        if seen.insert(tok.term.clone()) {
            query_terms.push(tok.term);
        }
    }

    let phrase_terms: Vec<(String, u32)> = phrase.map(|terms| analyze_phrase(terms, stem_lang)).unwrap_or_default();

    if query_terms.is_empty() && phrase_terms.is_empty() {
        return BTreeMap::new();
    }

    let mut result: BTreeMap<String, Vec<(u32, usize, usize)>> = BTreeMap::new();

    for &(fid, fname) in field_defs {
        let Some(val) = row.fields.get(&fid) else { continue };
        let mut elements: Vec<(u32, &str)> = Vec::new();
        match val {
            CanonicalValue::String(s) => elements.push((0, s.as_str())),
            CanonicalValue::Array(items) => {
                for (idx, item) in items.iter().enumerate() {
                    if let CanonicalValue::String(s) = item {
                        elements.push((idx as u32, s.as_str()));
                    }
                }
            }
            _ => continue,
        }

        let mut field_spans: Vec<(u32, usize, usize)> = Vec::new();
        for (elem_idx, text) in elements {
            let tokens = analyze_text(text, stem_lang);
            if tokens.is_empty() {
                continue;
            }
            let mut byte_spans: Vec<(usize, usize)> = Vec::new();

            // 1. Term matches (exact or prefix).
            for tok in &tokens {
                let matched = if prefix {
                    query_terms.iter().any(|qt| tok.term.starts_with(qt.as_str()))
                } else {
                    query_terms.contains(&tok.term)
                };
                if matched && tok.byte_start < tok.byte_end {
                    byte_spans.push((tok.byte_start, tok.byte_end));
                }
            }

            // 2. Phrase matches (position-aware, supporting co-located compound code tokens like THR-12).
            if !phrase_terms.is_empty() {
                let mut by_pos: BTreeMap<u32, Vec<&AnalyzedToken>> = BTreeMap::new();
                for tok in &tokens {
                    by_pos.entry(tok.position).or_default().push(tok);
                }
                let (ref first_term, first_rel) = phrase_terms[0];
                for (&start_pos, first_toks) in &by_pos {
                    let Some(anchor) = start_pos.checked_sub(first_rel) else {
                        continue;
                    };
                    let Some(first_tok) = first_toks.iter().find(|t| t.term == *first_term) else {
                        continue;
                    };
                    let mut span_start = first_tok.byte_start;
                    let mut span_end = first_tok.byte_end;
                    let mut matched = true;
                    for (pt, rel_pos) in &phrase_terms[1..] {
                        let target_pos = anchor.saturating_add(*rel_pos);
                        if let Some(toks) = by_pos.get(&target_pos)
                            && let Some(t) = toks.iter().find(|t| t.term == *pt)
                        {
                            span_start = span_start.min(t.byte_start);
                            span_end = span_end.max(t.byte_end);
                        } else {
                            matched = false;
                            break;
                        }
                    }
                    if matched && span_start < span_end {
                        byte_spans.push((span_start, span_end));
                    }
                }
            }

            if byte_spans.is_empty() {
                continue;
            }
            byte_spans.sort_unstable();
            let mut merged: Vec<(usize, usize)> = Vec::with_capacity(byte_spans.len());
            for (s, e) in byte_spans {
                if let Some(last) = merged.last_mut()
                    && s <= last.1
                {
                    last.1 = last.1.max(e);
                } else {
                    merged.push((s, e));
                }
            }
            for (s16, e16) in map_utf8_spans_to_utf16(text, &merged) {
                if s16 < e16 {
                    field_spans.push((elem_idx, s16, e16));
                }
            }
        }

        if !field_spans.is_empty() {
            result.insert(fname.to_owned(), field_spans);
        }
    }

    result
}

/// Exhaustive brute-force BM25F reference scorer over `TAG_DOCS` (used in tests to verify BMW exactness).
pub fn search_exhaustive_reference<R: KvRead>(
    txn: &R,
    db: SubDbHandle,
    query: &str,
    stem_lang: Option<&str>,
    top_k: usize,
    require_all: bool,
    candidates: Option<&FxHashSet<EntityKey>>,
) -> Result<Vec<TextHit>> {
    if top_k == 0 {
        return Ok(Vec::new());
    }
    let Some(stats_raw) = txn.get(db, &[TAG_STATS])? else {
        return Ok(Vec::new());
    };
    let stats = TextStats::decode(stats_raw)?;
    if stats.doc_count == 0 {
        return Ok(Vec::new());
    }
    let avgdl = (stats.total_weighted_len as f64 / stats.doc_count as f64).max(1.0);

    let mut terms: Vec<(String, f64)> = Vec::new();
    let mut seen: FxHashSet<String> = FxHashSet::default();
    for tok in analyze_text(query, stem_lang) {
        if seen.insert(tok.term.clone()) {
            let df = match txn.get(db, &dict_key(&tok.term))? {
                Some(raw) => TermDictEntry::decode(raw)?.doc_freq,
                None => 0,
            };
            if df == 0 {
                if require_all {
                    return Ok(Vec::new());
                }
                continue;
            }
            terms.push((tok.term, bm25_idf(stats.doc_count, df)));
        }
    }
    if terms.is_empty() {
        return Ok(Vec::new());
    }

    let mut hits = Vec::new();
    let mut cur = txn.cursor(db)?;
    let mut step = cur.seek_ge(&[TAG_DOCS])?;
    while let Some((k, v)) = step {
        if k.len() != 9 || k[0] != TAG_DOCS {
            break;
        }
        let ek = EntityKey::from_be_bytes(k[1..9].try_into().expect("8"));
        if candidates.is_none_or(|set| set.contains(&ek)) {
            let doc = StoredDoc::decode(v)?;
            let mut score = 0.0;
            let mut matched = 0usize;
            for (term, idf) in &terms {
                if let Ok(idx) = doc.terms.binary_search_by(|(t, _, _)| t.as_str().cmp(term.as_str())) {
                    let wtf = doc.terms[idx].1;
                    score += bm25_term_score(*idf, wtf, doc.weighted_doc_len, avgdl);
                    matched += 1;
                }
            }
            if (require_all && matched == terms.len()) || (!require_all && matched > 0) {
                hits.push(TextHit { key: ek, score });
            }
        }
        step = cur.next()?;
    }
    hits.sort_by(|a, b| b.score.total_cmp(&a.score).then_with(|| a.key.cmp(&b.key)));
    hits.truncate(top_k);
    // Use Reverse just so unused import warning never trips
    let _ = Reverse(0u8);
    Ok(hits)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf16_highlight_mapper_handles_ascii_bmp_and_surrogate_pairs_in_one_pass() {
        let text = "Hi 👋 café 🚀!";
        // "Hi " = 3B / 3 UTF-16
        // "👋" = 4B / 2 UTF-16
        // " " = 1B / 1 UTF-16
        // "café" = 5B / 4 UTF-16 ('é' is 2B / 1 UTF-16)
        // " " = 1B / 1 UTF-16
        // "🚀" = 4B / 2 UTF-16
        // "!" = 1B / 1 UTF-16
        let cafe_start = text.find("café").unwrap();
        let cafe_end = cafe_start + "café".len();
        let rocket_start = text.find("🚀").unwrap();
        let rocket_end = rocket_start + "🚀".len();

        let spans = map_utf8_spans_to_utf16(text, &[(rocket_start, rocket_end), (cafe_start, cafe_end)]);
        assert_eq!(spans, vec![(11, 13), (6, 10)]);
    }

    #[test]
    fn malformed_stored_doc_and_posting_block_fail_closed_without_panic() {
        // 1. StoredDoc with tlen = u64::MAX must not overflow `pos + tlen`!
        let mut bad_doc = vec![DOC_RAW];
        write_varint(1, &mut bad_doc); // weighted_doc_len
        write_varint(0, &mut bad_doc); // fl_count
        write_varint(1, &mut bad_doc); // term_count
        write_varint(u64::MAX, &mut bad_doc); // malicious tlen
        let err = StoredDoc::decode(&bad_doc).unwrap_err();
        assert_eq!(err.code, ErrorCode::Corrupted);

        // 2. PostingBlock with offset >= 128 must fail closed!
        let mut bad_blk = Vec::new();
        bad_blk.extend_from_slice(&1u32.to_le_bytes()); // max_tf
        bad_blk.extend_from_slice(&1u64.to_le_bytes()); // min_len
        bad_blk.extend_from_slice(&1u16.to_le_bytes()); // count = 1
        bad_blk.push(128u8); // out-of-bounds offset (>= 1 << BLOCK_SHIFT)
        write_varint(1, &mut bad_blk);
        write_varint(1, &mut bad_blk);
        assert_eq!(PostingBlock::decode(&bad_blk).unwrap_err().code, ErrorCode::Corrupted);

        // 3. PostingBlock with duplicate offset (delta == 0 on i > 0) must fail closed!
        let mut dup_blk = Vec::new();
        dup_blk.extend_from_slice(&1u32.to_le_bytes());
        dup_blk.extend_from_slice(&1u64.to_le_bytes());
        dup_blk.extend_from_slice(&2u16.to_le_bytes());
        dup_blk.push(5u8);
        write_varint(1, &mut dup_blk);
        write_varint(1, &mut dup_blk);
        dup_blk.push(0u8); // zero delta on 2nd entry
        write_varint(1, &mut dup_blk);
        write_varint(1, &mut dup_blk);
        assert_eq!(PostingBlock::decode(&dup_blk).unwrap_err().code, ErrorCode::Corrupted);
    }

    #[test]
    fn analyze_phrase_preserves_words_longer_than_128_bytes() {
        let long_word = "a".repeat(MAX_TERM_BYTES + 10);
        let single_str = vec![format!("hello {long_word}")];
        let terms = analyze_phrase(&single_str, None);
        assert_eq!(terms, vec![("hello".to_owned(), 0), (long_word.clone(), 1)]);

        let split_str = vec!["hello".to_owned(), long_word.clone()];
        assert_eq!(analyze_phrase(&split_str, None), vec![("hello".to_owned(), 0), (long_word.clone(), 1)]);

        let code_and_long = vec![format!("THR-12 {long_word}")];
        assert_eq!(
            analyze_phrase(&code_and_long, None),
            vec![("thr".to_owned(), 0), ("thr-12".to_owned(), 0), ("12".to_owned(), 1), (long_word, 2)]
        );
    }
}
