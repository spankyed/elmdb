# Diagnostics: what they report and what they cost

The evidence for [stage 5 of the development libraries](../roadmap/development-libraries.md#5-structured-operational-diagnostics).
2026-10-30, on `session/diagnostics` from `6eeccaa`, Rust 1.99.1, `tracing` 0.1.44. Instruction counts in
`scripts/linux.py`'s container (`rust:1-bookworm`, Linux arm64, two CPUs, 6 GiB) under Valgrind with gungraun 0.20.0;
timings on an Apple M1 Pro (macOS), Node 23.11.1. The commands are in `../architecture/testing.md`, Diagnostics.

## What is reported

Spans under the target `elmdb`, each ending with an `outcome` (`committed`, `durable`, `not_durable`, `begun`,
`returned`, `sorted`, `failed` with its `error` code as JavaScript's `error.code` names it, or `abandoned` for one
dropped before it ended, by an error returned past it or a panic) and `elapsed_us`. `Collector` gives each its `id`,
and its parent's `parent` and `parent_id`, so a take can be put back together as the tree it ran in:

| Span | Fields |
|---|---|
| `rebuild` | `entity_type`, `index` (both empty for a full rebuild) |
| `write` | `durable`, `waited_us` (until it held the writer), `growths` |
| `begin` | `waited_us` |
| `unit_wait` | none beyond its outcome and `elapsed_us` |
| `sort` | `rows`, `runs`, `spilled_bytes` (scratch bytes at its finish) |

A rebuild's write and sorts are its children. One run of `crates/elmdb-node/examples/diagnostics.mjs`, 1,000 notes
written and their value index rebuilt from Node:

```
{"durable":true,"waited_us":1,"outcome":"durable","elapsed_us":192,"span":"write"}
{"durable":false,"waited_us":3,"outcome":"committed","elapsed_us":10315,"span":"write"}
{"runs":1,"spilled_bytes":81634,"rows":1000,"outcome":"sorted","elapsed_us":2679,"span":"sort","parent":"rebuild"}
{"durable":true,"waited_us":1,"outcome":"durable","elapsed_us":3945,"span":"write","parent":"rebuild"}
{"entity_type":"Note","index":"title","outcome":"durable","elapsed_us":3974,"span":"rebuild"}
5 spans, 0 dropped
```

`tests/diagnostics.rs` asserts these shapes, that no field holds an id, a value or a word written, and that collecting
changes no answer.

## Off by default

Without the `diagnostics` feature, `elmdb` doesn't depend on `tracing` and every span is an empty struct whose methods
are empty. `tracing` is in every build already, through hannoy, so the check is on `elmdb`'s own dependency:
`cargo xtask check-diagnostics-off`. On a disposable copy, both ways a default build could turn the spans on failed it:
`default = ["diagnostics"]` in `elmdb`, and the binding depending on `elmdb` with `features = ["diagnostics"]`.

## Cost with nobody collecting

`scripts/diagnostics-overhead.py`, the feature off against on, one commit:

| Operation | Without | With, nothing collecting | Change |
|---|---:|---:|---:|
| `key_encode` (16, 128, 1,024 bytes) | 1,605 / 3,533 / 14,079 | same | 0 |
| `export` | 27,669,059 | 27,673,239 | +4,180 (+0.0151%) |
| `indexed_limit` | 325,675 | 325,675 | 0 |
| `value_rebuild` | 184,920,713 | 184,922,923 | +2,210 (+0.0012%) |
| `read_bytes` (field, page, projected, all rows) | 10,436 / 2,765,200 / 753,077 / 538,210,027 | — | 0, +24, −3, +3 |
| `small_write` | 1,187,939 | 1,187,992 | +53 (+0.0045%) |
| `text_rebuild` (reported only) | 913,959,664 | 913,647,270 | −312,394 (−0.0342%), the tokenizer's per-process seed |

A write's spans cost 53 instructions of its 1.19 million. The gate allows 0.05% or 100 instructions, whichever is more:
three times the largest growth measured.

**What the gate catches.** Each mutation applied inside the container's copy, gated, and the file restored:

| Mutation (compiled only with the feature) | Largest gated growth | Gate |
|---|---|---|
| (a) an uncollected `trace!` event per sorted entry | `value_rebuild` +196,175 (+0.106%) | fails |
| (b) a clock read per sorted entry | `value_rebuild` +508,175 (+0.275%), `export` +43,180 (+0.156%) | fails |
| (c) one clock read per write, outside the enabled check | `small_write` +88 (+0.0074%) | passes |

Work per row fails it; one more clock read an operation costs 35 instructions and passes, as a cost gate should. The
first run used 0.1%, which (a) crossed by only 6%, so the bound was set to 0.05% from these readings.

## Cost with a collector

The binding built with the feature, `collectDiagnostics()` on against off, three runs each in a fresh process: 20,000
one-field writes (`syncs: false`) took 17.6 to 17.9 µs each collected and 16.2 to 18.4 uncollected; a one-note value
index rebuild 596 to 748 µs collected and 520 to 627 uncollected. A reading on a desktop, not a bound: collecting costs
about a microsecond a write, within the runs' spread.

## Decision

Kept, behind the feature. Nothing collecting, the spans are within a twentieth of a percent of every benchmarked
operation, held there by a gate on every pull request. The binding's `collectDiagnostics()`/`takeDiagnostics()` is an
experiment behind its own feature, off by default, installing elmdb's `Collector`: a pull API, since spans close on
elmdb's threads rather than JavaScript's. Whether the release binding is built with the feature, and what a public collection API looks like, wait
for a stall that needs them; apack's use is proposed separately.
