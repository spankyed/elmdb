//! ICU Collation Order projection (1 sub-DB per index, 1-byte table tags).
//!
//! Layout inside the index's pooled `SubDbSlot`:
//! - `0x00` -> 32-byte `CollatorStamp` (SHA-256 of locale, layout, sort-key cap, and 24 probe sort keys)
//! - `0x01 || escaped_sort_key_prefix || 0x00 0x00 || cut(1B: 0|1) || EntityKey(8B BE)` -> `()`
//! - `0x02 || EntityKey(8B BE)` -> `()` (missing / null / non-string)

use std::sync::Arc;

use elmdb_engine::{EntityKey, FieldId, PackedRow, SystemDbs, read_packed_row};
use elmdb_storage::{KvCursor, KvRead, LockScopeId, MetaLock, SubDbHandle};
use elmdb_wire::{CanonicalValue, ElmdbError, ErrorCode, Result};
use icu_collator::{CollationKeySink, CollatorBorrowed, CollatorPreferences, options::CollatorOptions};
use icu_locale_core::Locale;
use rustc_hash::FxHashMap;
use sha2::{Digest, Sha256};

use crate::state::KvMut;
use crate::value::encode_order_preserving_bytes;

const TAG_STAMP: u8 = 0x00;
const TAG_ORDER: u8 = 0x01;
const TAG_MISSING: u8 = 0x02;

/// Sort-key byte cap leaving ample headroom under LMDB's 511-byte key limit even if every byte escaped.
pub const MAX_SORT_KEY_BYTES: usize = 240;

const LAYOUT_VERSION: u32 = 2;

const PROBES: [&str; 24] = [
    "", "a", "A", "b", "z", "ab", "a b", "a-b", "é", "e\u{301}", "ö", "å", "ß", "ı", "İ", "10", "9", "α", "я", "あ",
    "カ", "中", "한", "ก",
];

pub type CollatorStamp = [u8; 32];

pub struct CachedCollator {
    pub locale: String,
    pub collator: CollatorBorrowed<'static>,
    pub stamp: CollatorStamp,
}

/// Process-wide thread-safe cache of compiled ICU collators + stamps keyed by locale string.
pub struct CollatorCache {
    inner: MetaLock<FxHashMap<String, Arc<CachedCollator>>>,
}

impl Default for CollatorCache {
    fn default() -> Self {
        Self { inner: MetaLock::new_in_scope(LockScopeId::fresh(), FxHashMap::default()) }
    }
}

impl CollatorCache {
    pub fn get_or_build(&self, locale: &str) -> Result<Arc<CachedCollator>> {
        {
            let guard = self.inner.lock();
            if let Some(c) = guard.get(locale) {
                return Ok(Arc::clone(c));
            }
        }
        let collator = build_collator(locale)?;
        let stamp = compute_stamp(locale, &collator);
        let built = Arc::new(CachedCollator { locale: locale.to_owned(), collator, stamp });
        let mut guard = self.inner.lock();
        Ok(Arc::clone(guard.entry(locale.to_owned()).or_insert(built)))
    }
}

pub fn build_collator(locale: &str) -> Result<CollatorBorrowed<'static>> {
    let prefs = if locale == "root" || locale.is_empty() {
        CollatorPreferences::default()
    } else {
        let parsed: Locale = locale
            .parse()
            .map_err(|_| ElmdbError::new(ErrorCode::InvalidSchema, format!("invalid collation locale '{locale}'")))?;
        CollatorPreferences::from(&parsed)
    };
    CollatorBorrowed::try_new(prefs, CollatorOptions::default())
        .map_err(|e| ElmdbError::new(ErrorCode::InvalidSchema, format!("unsupported collation '{locale}': {e}")))
}

struct BoundedSink {
    bytes: Vec<u8>,
    room: usize,
}

struct SinkFull;

impl CollationKeySink for BoundedSink {
    type Error = SinkFull;
    type State = ();
    type Output = ();

    fn write(&mut self, (): &mut (), bytes: &[u8]) -> std::result::Result<(), SinkFull> {
        let left = self.room - self.bytes.len();
        if bytes.len() <= left {
            self.bytes.extend_from_slice(bytes);
            Ok(())
        } else {
            self.bytes.extend_from_slice(&bytes[..left]);
            Err(SinkFull)
        }
    }

    fn finish(&mut self, (): ()) -> std::result::Result<(), SinkFull> {
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SortKey {
    pub bytes: Vec<u8>,
    pub cut: bool,
}

pub fn sort_key(collator: &CollatorBorrowed<'static>, text: &str, room: usize) -> SortKey {
    let mut sink = BoundedSink { bytes: Vec::with_capacity(room.min(text.len().saturating_mul(2) + 8)), room };
    let cut = collator.write_sort_key_to(text, &mut sink).is_err();
    SortKey { bytes: sink.bytes, cut }
}

pub fn compute_stamp(locale: &str, collator: &CollatorBorrowed<'static>) -> CollatorStamp {
    let mut hash = Sha256::new();
    let mut part = |b: &[u8]| {
        hash.update((b.len() as u64).to_be_bytes());
        hash.update(b);
    };
    part(locale.as_bytes());
    part(&LAYOUT_VERSION.to_be_bytes());
    part(&(MAX_SORT_KEY_BYTES as u64).to_be_bytes());
    for probe in PROBES {
        part(&sort_key(collator, probe, usize::MAX).bytes);
    }
    hash.finalize().into()
}

fn order_key(collator: &CollatorBorrowed<'static>, text: &str, key: EntityKey) -> Vec<u8> {
    let sk = sort_key(collator, text, MAX_SORT_KEY_BYTES);
    let mut out = Vec::with_capacity(1 + sk.bytes.len() + 11);
    out.push(TAG_ORDER);
    encode_order_preserving_bytes(&sk.bytes, &mut out);
    out.push(u8::from(sk.cut));
    out.extend_from_slice(&key.to_be_bytes());
    out
}

fn missing_key(key: EntityKey) -> [u8; 9] {
    let mut out = [0u8; 9];
    out[0] = TAG_MISSING;
    out[1..9].copy_from_slice(&key.to_be_bytes());
    out
}

fn extract_str(row: &PackedRow, field: FieldId) -> Option<&str> {
    match row.fields.get(&field) {
        Some(CanonicalValue::String(s)) => Some(s.as_str()),
        _ => None,
    }
}

pub fn write_stamp<W: KvMut>(txn: &mut W, db: SubDbHandle, cached: &CachedCollator) -> Result<()> {
    txn.put(db, &[TAG_STAMP], &cached.stamp)?;
    Ok(())
}

pub fn stamp_matches<R: KvRead>(txn: &R, db: SubDbHandle, cached: &CachedCollator) -> Result<bool> {
    Ok(txn.get(db, &[TAG_STAMP])? == Some(cached.stamp.as_slice()))
}

pub fn apply_delta<W: KvMut>(
    txn: &mut W,
    db: SubDbHandle,
    cached: &CachedCollator,
    field: FieldId,
    key: EntityKey,
    before: Option<&PackedRow>,
    after: Option<&PackedRow>,
) -> Result<()> {
    // Enforce collator stamp on write: never silently mix keys from mismatched ICU collator versions!
    match txn.get(db, &[TAG_STAMP])? {
        None => write_stamp(txn, db, cached)?,
        Some(existing) if existing == cached.stamp.as_slice() => {}
        Some(_) => {
            return Err(ElmdbError::new(
                ErrorCode::IndexNotReady,
                format!("order index collator stamp mismatch for locale '{}'; rebuild required", cached.locale),
            ));
        }
    }
    let old_s = before.and_then(|r| extract_str(r, field));
    let new_s = after.and_then(|r| extract_str(r, field));
    if before.is_some() == after.is_some() && old_s == new_s {
        return Ok(());
    }

    if before.is_some() {
        match old_s {
            Some(s) => {
                txn.del(db, &order_key(&cached.collator, s, key))?;
            }
            None => {
                txn.del(db, &missing_key(key))?;
            }
        }
    }

    if after.is_some() {
        match new_s {
            Some(s) => {
                txn.put(db, &order_key(&cached.collator, s, key), &[])?;
            }
            None => {
                txn.put(db, &missing_key(key), &[])?;
            }
        }
    }
    Ok(())
}

/// Verifies that the persisted collator stamp matches `cached`, returning `IndexNotReady` on mismatch.
pub fn ensure_stamp_matches<R: KvRead>(txn: &R, db: SubDbHandle, cached: &CachedCollator) -> Result<()> {
    if !stamp_matches(txn, db, cached)? {
        return Err(ElmdbError::new(
            ErrorCode::IndexNotReady,
            format!("order index collator stamp mismatch for locale '{}'; rebuild required", cached.locale),
        ));
    }
    Ok(())
}

/// Streams `EntityKey`s in exact ICU collation order (resolving rare >240 B cut-key ties via primary lookup).
pub fn scan_ordered<R: KvRead>(
    txn: &R,
    dbs: &SystemDbs,
    db: SubDbHandle,
    cached: &CachedCollator,
    field: FieldId,
    reverse: bool,
    limit: Option<usize>,
) -> Result<Vec<EntityKey>> {
    let cap = limit.unwrap_or(usize::MAX);
    if cap == 0 {
        return Ok(Vec::new());
    }
    // Enforce collator stamp on read: refuse silently mis-ordered scans after ICU upgrades or corruption!
    ensure_stamp_matches(txn, db, cached)?;
    // Collect entries grouped by identical cut prefix so cut ties resolve cleanly.
    struct Entry {
        prefix: Vec<u8>,
        cut: bool,
        key: EntityKey,
    }
    let mut raw_entries: Vec<Entry> = Vec::new();
    {
        let mut cur = txn.cursor(db)?;
        let mut step = if !reverse {
            cur.seek_ge(&[TAG_ORDER])?
        } else {
            match cur.seek_ge(&[TAG_ORDER + 1])? {
                Some(_) => cur.prev()?,
                None => cur.last()?,
            }
        };
        while let Some((k, _)) = step {
            if k.first() != Some(&TAG_ORDER) {
                break;
            }
            if k.len() < 12 {
                return Err(ElmdbError::new(ErrorCode::Corrupted, "truncated order index key"));
            }
            let key = EntityKey::from_be_bytes(k[k.len() - 8..].try_into().expect("8"));
            let cut = k[k.len() - 9] != 0;
            let prefix = k[1..k.len() - 9].to_vec();
            raw_entries.push(Entry { prefix, cut, key });
            // Stop once we have `cap` entries and the next entry cannot tie with `raw_entries[cap - 1]` on a cut prefix.
            if raw_entries.len() == cap && !cut {
                break;
            }
            if raw_entries.len() > cap {
                let last_idx = raw_entries.len() - 1;
                if !raw_entries[last_idx].cut || raw_entries[last_idx].prefix != raw_entries[cap - 1].prefix {
                    raw_entries.pop();
                    break;
                }
            }
            step = if reverse { cur.prev()? } else { cur.next()? };
        }
    }

    // Resolve any adjacent cut-key ties using full collator comparison on the primary field value.
    let mut out = Vec::with_capacity(raw_entries.len().min(cap));
    let mut i = 0;
    while i < raw_entries.len() && out.len() < cap {
        let mut j = i + 1;
        if raw_entries[i].cut {
            while j < raw_entries.len() && raw_entries[j].cut && raw_entries[j].prefix == raw_entries[i].prefix {
                j += 1;
            }
        }
        if j - i == 1 {
            out.push(raw_entries[i].key);
        } else {
            let mut group: Vec<(EntityKey, String)> = Vec::with_capacity(j - i);
            for e in &raw_entries[i..j] {
                let s = read_packed_row(txn, dbs, e.key)?
                    .and_then(|r| extract_str(&r, field).map(ToOwned::to_owned))
                    .unwrap_or_default();
                group.push((e.key, s));
            }
            group.sort_by(|(ka, sa), (kb, sb)| {
                let ord = cached.collator.compare(sa, sb);
                let ord = if reverse { ord.reverse() } else { ord };
                ord.then_with(|| if reverse { kb.cmp(ka) } else { ka.cmp(kb) })
            });
            for (k, _) in group {
                out.push(k);
                if out.len() >= cap {
                    break;
                }
            }
        }
        i = j;
    }
    Ok(out)
}

/// Scans entities stored in `TAG_MISSING` (missing, null, or non-string field values) in `EntityKey` order.
pub fn scan_missing_keys<R: KvRead>(
    txn: &R,
    db: SubDbHandle,
    cached: &CachedCollator,
    reverse: bool,
    limit: Option<usize>,
) -> Result<Vec<EntityKey>> {
    let cap = limit.unwrap_or(usize::MAX);
    if cap == 0 {
        return Ok(Vec::new());
    }
    ensure_stamp_matches(txn, db, cached)?;
    let mut out = Vec::new();
    let mut cur = txn.cursor(db)?;
    let mut step = if !reverse {
        cur.seek_ge(&[TAG_MISSING])?
    } else {
        match cur.seek_ge(&[TAG_MISSING + 1])? {
            Some(_) => cur.prev()?,
            None => cur.last()?,
        }
    };
    while let Some((k, _)) = step {
        if k.first() != Some(&TAG_MISSING) {
            break;
        }
        if k.len() != 9 {
            return Err(ElmdbError::new(ErrorCode::Corrupted, "truncated order missing key"));
        }
        out.push(EntityKey::from_be_bytes(k[1..9].try_into().expect("8")));
        if out.len() >= cap {
            break;
        }
        step = if reverse { cur.prev()? } else { cur.next()? };
    }
    Ok(out)
}
