# Read allocations, copies and codecs: readings

The evidence for [Read allocations, copies and codecs](../roadmap/read-allocation-codec.md). 2026-10-29, on
`session/read-allocation` from `1268f34`. Machine: Apple M1 Pro, 10 cores, 32 GiB, macOS 26.3.1; rustc 1.99.1;
samply 0.13.1. The machine was running other work throughout: load averages 9 to 15, recorded with each reading,
so a comparison alternates its two sides rather than trusting one run of each.

## The fixture and the four reads

`crates/elmdb/tests/support/reads.rs`: 10,000 messages over 100 channels of 100, each with a channel (value-indexed),
title, sender, a body of about a kilobyte of repeated words (stored zstd-compressed, past `COMPRESS_FROM`), a nested
map, a list of three strings, 64 bytes, a number, a boolean, and a note that is null on every third and missing on
every fifth. The reads, each answer checked before it is timed:

| Read | Request |
|---|---|
| `field` | one entity's `title` |
| `page` | `where channel = "channel-0007"`, `limit 50`, whole rows |
| `page_projected` | the same page, `title` and `sender` |
| `all_rows` | every message's whole row |

Commands: `cargo bench -p elmdb --bench reads` (Criterion, each read as the binding makes it, `Database::read_bytes`
from the request's msgpack bytes to the answer's); `target/profiling/examples/read-allocations` (dhat: allocations per
read over 100 reads, 3 for `all_rows`); `samply record --save-only --unstable-presymbolicate -- target/profiling/examples/profile-reads WORKLOAD N`;
`node crates/elmdb-node/bench/reads.mjs` after `npm run build` (medians of `readSync` and of msgpackr's decoding apart).

## Baseline

On `1268f34` plus the benchmark files. Criterion, load 15:

| Read | Time | Answer bytes | Allocations a read | Bytes allocated a read |
|---|---|---|---|---|
| `field` | 1.03 µs | 40 | 14 | 571 |
| `page` | 307 µs | 65,677 | 5,174 | 6,975,796 |
| `page_projected` | 82 µs | 2,795 | 1,155 | 45,916 |
| `all_rows` | 58.3 ms | 13,150,385 | 994,053 | 1,402,353,634 |

Through the binding, load 9: `readSync` 2.1 µs, 340 µs, 80.6 µs and 64.0 ms; msgpackr's decoding of the answer
0.4 µs, 80.3 µs, 11.6 µs and 21.5 ms. The crossing itself adds about a microsecond.

A whole row allocates about 140 KB for an answer of about 1.3 KB.

## Where the time goes

samply, line tables on (`--profile profiling`), share of the samples inside `Database::read`:

- `page` and `all_rows` (alike): `entities::fields` 74 to 78% inclusive, of it `values::unstored` 29 to 32%;
  `ZSTD_freeDCtx` 12%; the kernel's reclaim of freed large blocks (`mach_vm_reclaim_update_kernel_accounting_trap`)
  20% self; vector growth (`RawVecInner::finish_grow`) 15%; the scan of the entity's fields 19 to 21%, its key
  decoding (`format::key::decode`) 16%.
- `page_projected`: LMDB lookups, one `mdb_get` a field, 51%; the value index's walk 38%. The work the request asks for.

`values::unstored` decompresses with `zstd::stream::decode_all`: a streaming decoder for each value, so a decompression
context and its input buffer allocated and freed for every compressed field, and the output grown as it is read. That
is the 140 KB a row, the frees the kernel reclaims, and most of the growth. Then `entities::fields` copies every
uncompressed value once more (`Cow::into_owned` of bytes the scan already copied) before the answer copies it again.

## Candidates

Declared before either is built.

**1. Decompress into the size the frame records, with a context reused per thread.** `values::unstored` decompresses
a frame that records its size, up to 16 MiB, in one call into a buffer of exactly that size, with one
`zstd::bulk::Decompressor` kept per thread; a frame that records none, or more, keeps the streaming decoder, so a
damaged header can't ask for a large allocation. Invariants: the same bytes out, a damaged frame still `CORRUPTED`, no
state carried between values (each call decompresses one whole frame), the context reachable only from its own
thread. Retained: one context a thread, about 100 KB. Predicted: most of the 140 KB a row, the context frees and the
growth: `page` and `all_rows` at least 20% faster to keep it, `field` and `page_projected` no slower past their noise.

**2. Keep an uncompressed value's scanned bytes.** `entities::fields` moves the bytes the scan returned where the value
isn't compressed, rather than copying them. Invariants: the same bytes, the same order. Predicted small: a copy of
the uncompressed fields' bytes a row. Kept only if `all_rows` is faster by more than its run-to-run spread, measured
alternately; otherwise reverted and recorded.

Out of these: the key decoding and the scan's own copies are the field loop's structure (a field's name is its key's),
and the projected page's cost is the lookups it asks for, which [Query work efficiency](../roadmap/query-work-efficiency.md)
owns.
