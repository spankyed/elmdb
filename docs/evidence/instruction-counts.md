# Instruction counts: readings

The evidence for [stage 7 of the verification tooling](../roadmap/verification-tooling.md#7-instruction-count-benchmarks).
2026-10-30, on `session/instruction-counts` from `af3f0f0`, in a `rust:1-bookworm` container on an Apple M1 Pro
(aarch64 Linux, four CPUs and 6 GiB given to it), Rust 1.99.1, Valgrind 3.19.0, gungraun 0.20.0 and its runner. The
host's load doesn't enter a count, which is the point; it is recorded because a run's duration does move with it.

## What is counted

`crates/elmdb/benches/instructions.rs`, the operations the Criterion benchmarks time:

- key encoding at 16, 128 and 1,024-byte strings;
- a value rebuild, a text rebuild, an export and an indexed query with a limit, over 1,000 entities;
- each of the four reads of the read benchmark, over 10,000 messages.

Each case is one process under Callgrind. Its setup builds the fixture and checks the answer outside the count, and
only the benchmark's own thread is counted.

## Two runs of one commit

Every count was the same in both runs but one:

| Operation | Instructions |
|---|---:|
| `key_encode` 16 / 128 / 1,024 bytes | 1,605 / 3,533 / 14,079 |
| `value_rebuild` | 179,102,097 |
| `export` | 26,422,274 |
| `indexed_limit` | 319,875 |
| `read_bytes` `field` / `page` / `page_projected` / `all_rows` | 10,096 / 2,746,225 / 743,205 / 513,763,313 |
| `text_rebuild` | 891,341,827, then 891,941,100 (+0.067%) |

The text rebuild's difference, function by function from the two Callgrind profiles, is all in the tokenizer:
charabia's `is_nonspacing_mark` and a `HashMap<&str, ()>` lookup under `std`'s `RandomState`. Its hash keys are seeded
per process, so the work of a lookup varies between processes, and the text rebuild moves by a fraction of a percent
between runs of one commit. Anything that tokenizes text shares it.

Duration: 296 s for the first run, its build from a cold target included, and 259 s for the second, which built
nothing. Each case's setup runs under Valgrind too, its fixture built there and not counted.

## A deliberate slowdown

On a disposable copy, `format::key::encode` encoded its parts once more and discarded the result, then the copy was run
against the second run above:

| Operation | Change |
|---|---:|
| `key_encode` 16 / 128 / 1,024 bytes | +89.8% / +91.3% / +99.3% |
| `text_rebuild` | +3.58% |
| Every other operation | none |

The key-encoding cases nearly double. The text rebuild, which encodes its entries' keys through `encode`, grows by
3.6%, well beyond its 0.07% between runs. The others reach no `encode` in what is counted, and didn't move.

## Not gated

A count is reported in the pull request's step summary and never fails it. A gate waits until it is known how much the
counts move with a toolchain or Valgrind update, which only the runs to come will show.
