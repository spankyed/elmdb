# plain-lmdb

Plain LMDB for Node: a thin, synchronous N-API binding over stock upstream LMDB, written in Rust with napi-rs.

It binds LMDB as OpenLDAP ships it, vendored and unmodified, and adds nothing to the database itself. No custom
allocator, no caching, no async write queue. A bug in the storage engine is then LMDB's, with LMDB's review history
behind the fix.

## What it does

- Open an environment, read and write transactions, get, put, delete, drop, range scans (forward, reverse, bounds,
  limit, keys only), copy (optionally compacted), sync off the main thread, reader checks and stats.
- Scoped buffers: a read hands back views into the map with no copy, detached when its transaction ends, so a view
  used afterwards throws instead of reading other data.
- Batched scans: one call returns many entries, since crossing into native code per entry is most of a scan's cost.
- A crash record: LMDB aborts on a failed assertion, and nothing can turn that into an exception, so the binding
  writes what failed before the abort for the app to report on its next start.

## Status

Not started. The first step is a spike of the read path (scoped buffers and batched scans) measured against lmdb-js
on a real database.
