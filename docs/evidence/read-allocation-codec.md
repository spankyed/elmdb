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

## Results

Each comparison alternates a disposable copy of the source before the change with one after it, each with its own
target directory, Criterion medians, load averages 8 to 19 through them.

**1. Kept** (`574797f`). Three runs a side against `89ecb6c`:

| Read | Before | After |
|---|---|---|
| `field` | 1.02 to 1.04 µs | 1.00 to 1.03 µs |
| `page` | 304 to 308 µs | 222 to 228 µs, 27% faster |
| `page_projected` | 78.9 to 80.2 µs | 80.2 to 84.6 µs, a path that decompresses nothing |
| `all_rows` | 58.6 to 59.5 ms | 42.1 to 45.0 ms, 27% faster |

Bytes allocated a read: `page` 6,975,796 to 422,046, `all_rows` 1,402,353,634 to 91,603,634.
`a_frame_with_or_without_its_size_or_past_one_call_reads_back_and_one_cut_short_is_corrupted`
(`engine/tests/compression.rs`) reads back a frame that records its size, one that records none and one past 16 MiB,
and refuses a frame cut short as `CORRUPTED`; in disposable copies it fails with the streaming path removed and with
the one-call path's error ignored. CI's `mutants` job left four survivors on the one-call limit: two that stream
every value, excluded in `.cargo/mutants.toml` since the stream answers the same bytes, and two that would allocate
whatever a frame claims, now killed by
`a_frame_claiming_more_than_any_buffer_holds_is_corrupted_without_allocating_its_claim`, a frame claiming 2^63 bytes.

**2. Kept** (`63ba2f3`). Four runs a side against `574797f`: `all_rows` 42.6 to 43.4 ms before, 40.9 to 41.9 after,
every run after faster than every run before (4% on the means, against a 0.8 ms spread before); `page` 221 to 236 µs
to 214 to 220; `page_projected`, whose per-field reads lose the same copy, 80.6 to 87.5 µs to 77.8 to 79.6; `field`
unchanged. Allocations a read: `page` 5,124 to 4,684, `all_rows` 984,053 to 896,053.

Through the binding after both, load 9: `readSync` 2.0 µs, 251 µs, 80.5 µs and 46.4 ms, from 2.1, 340, 80.6 and 64.0;
msgpackr's decoding unchanged, 0.4 µs, 79.6 µs, 11.9 µs and 21.7 ms, which apack's companion plan owns.

The page's profile after: `values::unstored`, `ZSTD_freeDCtx` and the kernel's reclaim are out of its top; the field
scan is 25%, the key decoding 22% and vector growth 21%. Those are the next costs, outside this plan's two candidates:
reopen it on a read whose profile they lead.

Checks on `63ba2f3`: `cargo fmt`, workspace clippy, `cargo nextest run --workspace` (339 passed), doc tests,
`check-generated`, `check-one-lmdb`, the binding's build, typecheck and 37 tests.
