# Phase 10 of goal-elmdb-next: the designs and their readings

2026-10-28, elmdb `6fdb065`, apack `a983092538`. Each design is its roadmap document, "Status: designed", with its
format, API, risks and phases: `docs/roadmap/plan-algebra.md`, `change-log.md`, `incremental-views.md`,
`javascript-boundary.md` and `value-dictionaries.md`. The readings below are what they rest on; those already recorded
by Phases 3, 4, 6 and 9 are cited where they are.

## The data

G8's dump loaded by the `elmdb` CLI at `7dd8a7f`, then opened under the schema apack's registered packs declare
(`toElmdbSchema` over default-setup's registration and the SDK's and host's types, as Phase 5's audit did), so its
value indexes are built. The dump's short codes carry older prefixes than the app declares, and a prefix can't change
once rows hold codes, so each type's prefix was taken from its stored codes. `check()` found no difference and the 451
violations Phase 5 recorded (`next-phase-5.md`). Machine: Apple M1 Pro, load 10 to 26 over the readings; release
binding, medians of seven unless stated.

## Plan algebra

- **Value indexes**: `elmdb inspect` on the data counts 13 value indexes and 851 entries — `Thread.shortCode` 343,
  `Note.shortCode` 280, `Action.shortCode`, `.category` and `.label` 69 each, the rest under ten.
- **Two filters over 100,000 tasks**, a disposable spec in a copy-on-write clone (release build): `status` value-indexed,
  half `open`; `prio` value-indexed, a fifth `high`; 10,000 both.

| Read | Time |
|---|---:|
| `where(status, open).where(prio, high)`, the larger index driving | 58.7 ms |
| `where(prio, high).where(status, open)`, the smaller driving | 26.2 ms |
| `where(prio, high)` alone | 12.9 ms |
| the two values' places as roaring bitmaps (16,423 bytes each), decoded and intersected | 0.005 ms |
| the same, and each of the 10,000 places turned into its id | 4.4 ms |

## The change log

`node p10-messages.cjs` over every Message's text, sender, timestamp and blocks:

- 19,681 messages from 2026-04-17 to 2026-09-04, on 96 days; 15,757 the assistant's, holding 4.6 MB of text and
  138 MB of blocks; 3,911 the user's, 2.1 MB of text. A day's final values: 0.86 MB at the median, 8.8 MB the most.
- **A log of whole-field writes**, replayed from the final values as apack writes them (`updateMessageState`):
  a message's text written whole once per 13 bytes streamed (Phase 3's long reply: about 400 words, taken as 2.4 KB, in 181 writes); a
  tool-activity block written whole as each of its entries starts and again as it ends; every other block once. That
  log holds 8.16 GB for 142.5 MB of final values, 57 times; with the blocks counted as written once each it would be
  503 MB, 3.5 times, so the tool-activity rewrites are most of it. The largest such block holds 57 entries, 3.3 MB.

## Incremental views and the boundary

`node p10-boundary.cjs`:

| Read | Answer | Engine and crossing | Decode in JavaScript |
|---|---:|---:|---:|
| every Message, whole rows | 148.0 MB | 265.8 ms | 283.5 ms |
| every Message, `text`, `sender`, `timestamp` | 8.2 MB | 67.7 ms | 18.1 ms |
| every Message's id | 0.6 MB | 4.2 ms | 0.5 ms |

The thread with the most messages, 1,647: its messages read whole through its `contains` edges, read and decoded,
8.9 ms (483 KB); its first 50, 0.68 ms. A chat turn's watch runs and crossings are Phase 3's (`next-phase-3.md`).

## Dictionaries

`node p10-dict.cjs` wrote every field value under 256 bytes of msgpack, per type with at least 1,000 of them, as one file
each, every other value to a training set and the rest to a test set; `zstd --train --maxdict=16384` trained a
dictionary per type; `zstd -b3 -r` and `zstd -b3 -D <dict> -r` compressed the test set a value at a time:

| Type | Values | Test bytes | Alone | With the dictionary |
|---|---:|---:|---:|---:|
| Message | 148,705 | 1,023,952 | 1,635,080 | 1,652,228 |
| Artifact | 6,652 | 61,870 | 91,408 | 81,707 |
| Thread | 3,244 | 17,701 | 32,239 | 35,886 |
| Note | 2,915 | | | |
| Terminal | 1,360 | | | |

Note and Terminal's training sets were smaller than a 16 KB dictionary and zstd warned the dictionary would be poor;
they are listed for their counts. Node's `zlib.zstdCompressSync` with a `dictionary` option gave the same sizes with
and without it, so its figures were discarded for the CLI's.

## The probes

Kept in the session's scratch directory, not committed, as earlier phases' were: `p10-schema.mts` (run from apack's
gitignored `drive/`, the schema and the open), `p10-messages.cjs`, `p10-boundary.cjs`, `p10-dict.cjs`, and the
two-filter spec, which lived only in the clone.
