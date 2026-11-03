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

## The rank drain

`rank_drain`, an edge linked and unlinked, each drained, over spread links at 1,000 documents and near ones at 4,000, counted from
`build_rank` on the index's thread. A fixture's build goes through `build_rank` too, so `rank_build` counts that alone, and the
drain is one less the other. The entry point names the function and not the closures in it: a `*build_rank*` pattern matched them
as well, which toggled collection off for their length and left out a drain's writes, reading 15.6 million instructions for what
is 43 million. In the Linux container, 2026-11-03:

| Case | Runs of one commit | Their spread |
|---|---:|---:|
| `rank_drain`, spread | 92,832,565, 92,832,525, 92,832,512 | 53 |
| `rank_build`, spread | 49,868,522, 49,868,566 | 44 |
| `rank_drain`, near | 215,378,347, 215,378,357, 215,378,334 | 23 |
| `rank_build`, near | 191,116,801, 191,116,789, 191,116,800 | 12 |

So a drain is about 43.0 million over spread links and 24.3 million over near ones. A drain over every node sharing its arc read
with the read of a few, a regression the timings of 2026-11-02 found only by bisecting, reads 44.9 million over spread links on a
copy, +4.4%, where at 100,000 documents it cost 80% of the arc read's time; some of that no instruction count sees. Over near links it reads about the same, as that drain doesn't take that path, and its `rank_build` +0.2%; over spread links `rank_build` reads +2.9%, as the fixture's own drain takes that path.

## The machine

`read_bytes` `all_rows` read one of two counts in CI, about 524.95 million or 530.17 million, whatever the commit. Over about 70
CI jobs the mode stayed within a job and moved between jobs at the same minute, so it wasn't the clock; one
binary at 100%, about 50% and about 25% of a core read within 24 instructions, so it wasn't load (aarch64 container).
A workflow ran one binary, by its SHA-256, three times each on eight runners on 2026-11-03:

| CPU | Runners | `all_rows` |
|---|---:|---:|
| AMD EPYC 7763 | 4 | 530,169,830 to 530,169,833 |
| Intel Xeon Platinum 85xx | 1 | 530,169,830 |
| Intel Xeon Platinum 83xx | 1 | 530,169,830 |
| AMD EPYC 9V45 | 2 | 524,950,622 |

Function by function from one job's two Callgrind profiles, every function of elmdb's reads the same and the 5.2 million
is in glibc's `_int_malloc`, `malloc_consolidate` and `_int_free`: the heap settles in another shape. The CPU reaches the
heap some way not traced here, and the binary with it: that job, whose cache size is the Xeon 85xx's, read low without the `diagnostics`
feature and high with it, which failed the overhead gate on a change of comments alone. Under Valgrind the heap also runs
out of its `brk` segment and falls back to `mmap`, which no process outside Valgrind does.

So the benchmark allocates from an address range that only moves forward, and `free` does nothing. An allocation costs a
fixed number of instructions whatever came before it: 24 on x86_64 and 30 on aarch64, each dividing `all_rows`' allocating
into the same 896,053 allocations. On `f613673`, over two runs of eight runners that met EPYC 7763, 9V45 and 9V74 and Xeon 8370C, 8573C and 6973P-C between them
(`instruction determinism` runs 41645890627 and 41707931411), `all_rows` read 395,193,893 on all sixteen, and the gated cases
differ by at most 3 instructions in each run.
The largest case allocates 777 MB in its whole process, setup included (aarch64), inside the 4 GiB it reserves.

What a count holds moved with it. On one aarch64 container `all_rows` fell from about 537 million to 349 million, and on
x86_64 `small_write` fell from 1.41 million to about 49,000, so most of a small write's count was glibc. 0.05% of the old
count, 706, had covered one operation's spans; of the new one it fell to the floor of 100, below the spans' 172 (`Op::live`
and its drop, aarch64). The floor is now 300, still under that 706.

Allocations are gated apart, by the allocator's own instructions, since an allocation at a few instructions is little in a
share of a count. On a copy with one allocation per row added under the feature, `all_rows` grew 189,985 instructions against a
bound of 174,586 and `page` 943 against 940, narrowly, and both failed on allocating, 300,000 and 1,500 instructions more:
10,000 and 50 allocations at 30 each. A case can grow less than its allocating did, the added code changing what the
compiler inlines: `all_rows`' `fields` fell 1,050,000 and its `write_row` rose 939,745. Callgrind's call counts can't do
this: they include the setup, so an 8,507-instruction read reported 1.1 million allocations.

Time still reaches a count one way that isn't the machine's CPU: a read within 100 ms of the setup's last reuses its
transaction (`store/idle_reads.rs`), and a later one opens another. No run has been seen to flip on it, but a stall could
make one, so each setup waits past that window.
