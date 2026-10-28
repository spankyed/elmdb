# Value compression with per-type dictionaries

Status: designed, 2026-10-28, and not to build on today's data: a dictionary can't make G8's small values smaller than
they are stored (`../evidence/next-phase-10.md`, Dictionaries). Written for the data that would change that.

## Where it stands

A field's value of 256 bytes of msgpack or more is stored as `0xc1` and a zstd frame (level 3) when that is smaller
(`../architecture/format.md`, Values; goal-elmdb-next's Phase 8): G8's file compacted from 267.8 MB to 122.1 MB. A
value under 256 bytes is stored as its msgpack. zstd compresses small inputs well only with a dictionary trained on
inputs like them, which is what this would add.

## The measurements

On G8's data under apack's schema (2026-10-28): every field value under 256 bytes, by type, split in halves, a
dictionary (at most 16 KB) trained with `zstd --train` on one half and the other compressed one value at a time at
level 3, by the zstd CLI's benchmark (each value its own frame):

| Type | Values | The measured half's bytes | Compressed alone | With the dictionary |
|---|---:|---:|---:|---:|
| Message | 148,705 | 1,023,952 | 1,635,080 (1.6× larger) | 1,652,228 (1.6× larger) |
| Artifact | 6,652 | 61,870 | 91,408 (1.5×) | 81,707 (1.3×) |
| Thread | 3,244 | 17,701 | 32,239 (1.8×) | 35,886 (2.0×) |

The small values average 14 bytes — numbers, times, enums, short ids — and a frame's header, about 13 bytes with a
dictionary's id, is as large as most of them. Taking that header off (zstd's raw blocks, the dictionary named by the
type) would by that estimate save about a third of Message's 2 MB: under 1% of the file, where each such value's LMDB key and
node already cost more than the value.

## The design

For a type whose small text values are measured to be a share of the file:

- **A dictionary per type**, trained by `rebuild()` or an explicit `db.trainDictionary(type)` on a sample of the type's
  values between a lower bound (64 bytes) and `COMPRESS_FROM`, stored as `m/z type → { id, bytes }`. A type's
  dictionaries are versioned by `id`; a new training adds one, and values written under an older id stay readable.
- **The stored form**: `0xc2`, the dictionary's id (a msgpack integer), and a raw zstd block without a frame header,
  used when smaller than the msgpack; `0xc1` stays the dictionary-less frame. A reader decodes `0xc2` with the type's
  dictionary of that id, which an open loads once.
- **Never on keys**, which are their order; never on primary records other than field values, as today.

## Risks

- **A dictionary is primary data**: losing it loses every value written with it. It is written in the same unit as the
  first value that uses it, dumped with the schema, and `check()` refuses a `0xc2` value whose dictionary is missing.
- **Training cost and drift**: a dictionary trained on early data compresses later data worse; retraining doesn't
  rewrite old values, so a type may hold several dictionaries.

## Phases

1. **The trigger.** A type whose values between 64 and 256 bytes are over 10% of a compacted file, read by `elmdb
   inspect` extended with a histogram of value sizes per field. Done when the histogram is in `inspect` and recorded
   for G8's data and any larger corpus apack has.
2. **Dictionaries.** Only once phase 1 shows a type over its threshold. Done when: `0xc2` values read back exactly
   (the compression spec's readers: a row, a field, a filter, every index's derivation, `check()`, `rebuild()`, a
   dump); a missing dictionary is refused by `check()`; the type's compacted size and apack's bench are recorded
   before and after, and the size falls by more than the dictionary's own.
