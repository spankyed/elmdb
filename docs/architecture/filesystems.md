# Filesystem support and coverage

Status: 2026-11-16. This is a support policy and an evidence boundary, not a filesystem certification.

## Supported operating envelope

Keep an open database on a local filesystem that provides coherent shared memory mappings, working interprocess
locks, atomic same-filesystem publication and the synchronization operations requested by elmdb. Read-only data
access still requires writable LMDB reader registration in lock.mdb. A read-only mount without registration is refused.
A read-only open also takes elmdb's own lock files, `synced.lock` and `open.lock`, which it makes where they are
missing and is refused (`IO`) where it can't; one that exists and can only be read is locked as it is
(`store/record.rs`, `lock_file`). A read-only open that finds a restore to make writes it, and is refused
(`RESTORE_PENDING`) where it can't write the data file (`engine.md`, Durability).

A handle that listens for other processes' commits makes a FIFO in the database's `wake/` directory on Unix, so one
on a directory it can't write, or on a filesystem with no FIFOs, can't listen (`store/wake.rs`); it still reads, and
asks `committedElsewhere()` instead.

An OS appearing in the CI matrix does not establish support for every filesystem available on that OS. Network-mounted
live databases (including NFS/SMB), cloud-synchronized live database directories, and removable-media configurations
are outside the currently validated operating envelope. A filesystem that accepts a call has not thereby demonstrated
crash durability, correct remote locking or coherent mappings.

Backup, dump and load destinations must support the particular operation's write and synchronization contract.
For publishing a new database from a load:
Linux and macOS try an exclusive rename (`RENAME_NOREPLACE`, `RENAME_EXCL`) and fall back to atomic hard-link
creation only when the kernel or the filesystem doesn't support it, and Windows uses its native move operation
(`store/backup.rs`, `publish_new`). Existing targets are not overwritten by
new-file publication. A filesystem supporting neither publication method returns an error. No load-publication
fallback copies bytes into a publicly visible partial destination. A dump file itself is created exclusively, written and synced in place;
it can be visible while being written, and an ordinary failure removes it. Consume it only after dump succeeds.
A backup replaces the `data.mdb` its destination holds, by a rename over it, and its contract is `engine.md`'s (Admin,
`backup`); callers must exclusively own the backup destination against other processes.

For an untested storage destination, create and verify a complete backup/dump on validated local storage, then transfer
the closed artifact. Restore onto local storage and verify it before switching the application. Transporting a closed
artifact is distinct from opening a live LMDB database on that destination. Transfer-tool integrity and durability are
outside elmdb's tests.

## What the tests establish

| Configuration | Evidence | Limit |
|---|---|---|
| Linux glibc/musl, x64/arm64, CI runner/container local storage | Rust and Node operations, model/crash suites, restore/publication checks | Exact mount types/options are not recorded as a certified filesystem matrix |
| macOS arm64/x64, CI runner local storage | Same functional suites, native publication, process/worker teardown | No general APFS-volume, external-disk or power-loss guarantee |
| Windows x64, CI runner local storage | Same functional suites and published backup-attribute mutation proof | No general NTFS/exFAT/SMB or power-loss guarantee |
| Linux real read-only bind mount | Missing reader registration is refused; removing the check fails the test | Tests reader safety refusal, not general network-storage compatibility |
| Injected Linux unsupported-rename errors | Hard-link fallback works and refuses collisions; removing it fails the test | No actual NFS/SMB server was exercised |
| Linux, LazyFS at a pinned commit, each night (`.github/workflows/lossy-filesystem.yml`) | A writer dies in or between syncs, what it didn't sync is dropped, and the database opens at its last finished sync; four rounds tear a write to the data file; the run with the pin turned off must fail (`testing.md`, Power loss) | One Linux runner over a FUSE filesystem that models the loss; no device's own cache, and no macOS or Windows |
| NFS, SMB, exFAT and cloud-synchronized directories | No dedicated deployment test record | Unvalidated; not advertised as supported live-database storage |

SIGKILL tests establish process-crash behavior under the tested host. They do not emulate controller caches, server
failover or a disconnected network mount. A power loss is replayed on the data file (`engine/tests/power_loss.rs`)
and modelled by LazyFS, the row above; no machine's power has been cut under a test (`../roadmap/synced-commit.md`,
phase 5). The acceptance runs and exact revisions are linked from
[remediation evidence](../evidence/elmdb-remediation.md).

## Adding a supported configuration

Record OS/kernel, filesystem/server/protocol versions, mount options, architecture, storage/cache assumptions and
elmdb revision. Run interprocess reader/writer and lock-registration tests, publication/collision tests, sync-failure
handling, dump/load and backup/replacement verification, and bounded crash/reopen tests. Include disconnect/reconnect
or failover behavior where relevant. Demonstrate negative assertions on disposable mutations or controlled failures.

Keep distinct evidence for an open database, backup publication, dump publication and restoration; passing one does
not establish the others. Record what was actually tested and any unavailable failure modes before broadening support.
