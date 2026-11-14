# A power loss that tears no file

Status: designed, 2026-11-14, and not built. It is the answer to the one defect of the review of `ed20979` that was
left for a design (`../roadmap.md`, change 1): in the grouped mode a power loss can leave a file that is no commit's,
which reads wrongly or ends the process that reads it. This is the third draft. The first, a marker of commits not
yet synced with the file refused when the machine restarted under it, was read against the code by a second reader
and did not hold. The second was this design as that reader proposed it, taken a step further, and the same reader
read it against LMDB's source: what that changed is under What was set aside. This draft has had no outside read.

## Where it stands

In the grouped mode LMDB is opened with `MDB_NOSYNC`, and a thread syncs once writes pause for 100 ms and at most 1 s
after the first unsynced commit (`../architecture/engine.md`, Durability). Nothing orders the pages and the meta pages
written between two syncs, so a power loss can leave a meta page on disk without all the pages it names, and LMDB
opens a file by the newer of its two meta pages, whatever it names. An open refuses such a file where its trees reach
past its end (`store/reach.rs`, `CORRUPTED`). Torn within its length it is opened, and may read wrongly or abort.

More than the unsynced commits can go: once three commits follow a synced one, LMDB may give the third the pages
the synced commit's trees no longer share with its successor, so the synced commit itself can be written over before
anything after it is whole. The remedy today is a backup, or `syncEachCommit` at a sync for every commit.

A sync that fails is final until the database is opened again, since the OS may have dropped the pages it couldn't
write; opened again in the same boot the database goes on, on a file that may lack them, and until then the process
goes on committing over them.

## What LMDB already promises

A read transaction reads its commit's trees for as long as it lives, whatever is committed after: LMDB gives a
writer no page that a commit at or after the oldest reader's freed. A free record is taken only while its commit is
before `mdb_find_oldest`, which reads every live reader in the lock file, a writer's cached list of free pages
doesn't outlive its transaction, pages a transaction spills are its own, and a reader begun while a commit is being
written is at the commit before (`mdb.c`, `mdb_page_alloc`, `mdb_txn_end`, `mdb_page_spill`, `mdb_txn_renew0`; the
second reader verified each). It is what a snapshot is, and it holds across processes.

So a read transaction held at a commit that is on disk keeps that commit on disk: every page of its two trees, the
one of rows and the one of free pages, was synced, and none is written again while the transaction lives. Whatever a
power loss then leaves of the commits after it, the pages of that one are whole. What a loss can still take is the
meta page that names them, which the second commit after it writes over.

## The design

**The last synced commit is kept: its pages by a read transaction, its meta page by a file beside the data.** After a
power loss the database is opened at that commit: what was not synced is gone, as the grouped mode says it may be,
and what was is whole. No commit pays for it.

| What | Holds |
|---|---|
| The pin | a read transaction every process that writes in the grouped mode holds, at the commit the record names or an earlier one |
| `synced`, a file beside the data file | the record: the body of the meta page of the last commit known to be on disk, a checksum of the two root pages it names, the boot it was written in, the mode the database is written in, a state, and a checksum of them all |
| `synced.restored` | the two meta pages a restore wrote over, as they were |

- **The record is two slots written in turn, in place**, each with its own checksum, the later one that reads
  being the record: one sync for each record, no file made or renamed, so a full disk doesn't stop one, and a loss
  leaves the record before or the record after. The file is made, and its directory synced, by the first writable
  open; it is never replaced, so a lock on it holds. It is not LMDB's lock file, whose locks a process loses by
  closing any descriptor of it.
- **A meta page is taken whole.** A sync begins a read transaction and reads its commit's meta page; it takes it
  only if the file's other meta page is of an earlier commit, and otherwise begins again. A commit two after it
  writes that page field by field, and a read beside it could take one commit's free tree with another's rows.
- **A sync moves the pin, in this order**: the read transaction is begun and its meta page taken; the data file is
  synced; the record is written and synced; the transaction that was the pin is ended, and the new one is the pin.
  A commit a durable record names is so pinned from before it is named until a later record is durable.
- **A record is written by one process at a time and names only a later commit than the one before**, under a
  lock of the record's file between processes and a mutex within one, a lock between two descriptors of one
  process being no lock on every filesystem.
- **A process commits only while its pin is at or before the record's commit.** A writable open pins the commit it
  finds, syncs, and writes the record before its first commit. A process whose pin was ended to let the map be
  resized, grown by it or by another process (LMDB changes a map's size only while the process has no
  transaction), pins again, and syncs and writes the record first if the commit it finds is later than the
  record's. The pin's own beginning can meet a map another process grew, and takes it as any transaction does.
- **Every commit of a write transaction goes through one function**, which is where that rule is held: the open's
  own commits, the vector graphs' databases made at open and the checkpoint after a reset among them
  (`store/mod.rs`, `with_graphs`, `recycle_reset_pages`), which today commit beside it, the first of them synced
  by nothing. A repository check holds that no other commit of a write transaction is in the store or the engine,
  as `check-sync` holds the locks.
- **A reset keeps its order**: its commit, the checkpoint that lets a load use the pages it freed, then the pin at
  the checkpoint's commit, the sync and the record.
- **The boot** is the kernel's own identity of it: `/proc/sys/kernel/random/boot_id` on Linux, and where a sandbox
  hides it the wall time less the time since boot; `kern.bootsessionuuid` on Apple's systems; on Windows the wall
  time less the uptime, with a count of boots beside it if the system has one that holds through a fast startup,
  which is read before it is relied on. Where none can be read, a record is of no boot, which is another boot than
  any. The first open of a boot writes the record again with its boot, at the same commit.
- **The state** is `kept`, or `failed` once a sync fails.

**A database is written in one mode at a time.** The grouped mode keeps the pin and the record. A process that syncs
each commit, or none (`syncEachCommit`, `syncs: false`), keeps neither, and the record says which: `ordered`, under
which LMDB's own order of writes leaves the file some commit's after any loss, or `unsynced`, under which nothing is
known of it, as nothing is today. Every writable open holds a lock on the record's file for its mode, and an open in
another mode than a live writer's is refused (`MODE_MISMATCH`, a new code), naming both. So no writer without a pin
commits under a record that names a commit to restore. The first writable open in a mode writes the record for it,
synced, before its first commit; an open after a writer of another mode has gone pins, syncs and writes its own.

**What an open does**, before LMDB opens the file, holding the record's lock:

- *No record, or one that names a later commit than the file's newer meta page, or whose checksum of the root
  pages isn't the file's*: the file is not the record's. It is a database from before records, a copy a backup
  made, or a file put where another was. It is opened as it is, and a writable open writes its record.
- *An `ordered` or `unsynced` record*: opened as it is.
- *A `kept` record of this boot*: a process died, or closed. The OS holds what it wrote, so the newest commit is
  whole, and the database is opened at it.
- *A `kept` record of another boot*: the machine stopped. The file's newer meta page is the record's commit: every
  commit was synced, and the database is opened. It is a later commit: commits were made after the last sync, and
  what of them reached the disk is not known, so the record's commit is restored (below).
- *A `failed` record, of any boot*: the record's commit is restored.
- *A record neither of whose slots reads* is refused (`CORRUPTED`): each is written whole and one always stands.

**A restore** is the one write to a file LMDB owns. It is made only by a writable open that holds the record's file
alone, no other process having the database open: another process would go on from the commits being undone, its
readers on pages about to be given out again. An open that finds a restore to make and can't make it, a read-only
one or one beside another process, is refused (`RESTORE_PENDING`, a new code), saying to close the other processes
and open the database writable once. The restore saves the file's two meta pages beside it, writes the record's meta
body to both, from the map's size on as LMDB writes one, syncs the file, and writes the record again as of this boot
and `kept`: stopped at any step, the next open makes it again. LMDB then opens at the record's commit, and the
open's first commit moves the epoch, as a replacement of every row does, so nothing that held a sequence from the
commits that went takes a later one for it.

An open that restored a commit says so in its report (`OpenReport`): the sequence it opened at, and that commits
after it were made and are gone. A caller that needs each commit kept asks for it durable, or opens with
`syncEachCommit`, as now.

**A sync that fails** writes the record `failed`, in place, where a full disk doesn't stop it. From then the
process refuses every commit, as it refuses every wait today, and a process that finds the record `failed` at its
own sync does the same: only a restore writes over it. If the record can't be written either, the database goes on
as it does today, which the error says.

**What else writes the data file.** A backup made into a directory, and `replaceWith`, write the record for the data
they put there or remove it. `replaceWith` opens its source as an open does, record and all.

**An elmdb from before this design** keeps no pin and reads no record. A database it wrote to under a `kept` record
that then lost power is restored to a commit whose pages it may have written over; the checksum of the root pages
catches it where a root was written, and not below. It is the cost the format's rule accepts until a release has
users (`../architecture/format.md`, Versions).

## What it costs

- **Nothing on a commit's path.** A commit reads no file and waits for no sync.
- **A sync of the record after each sync of the data**, on the thread that syncs: two syncs where there was one. A
  `durable` write waits for both, about 8 ms where it was 4.0 on an Apple M1 Pro by `engine.md`'s reading of
  2026-11-12, which is what `syncEachCommit` costs there.
- **A reader's place in the lock file** for each process writing in the grouped mode.
- **Pages freed since the last record wait for the next before they are used again.** The wait is the time
  between syncs, a second at most, and the time the two syncs take. A file's length grows by what its commits free
  in that time and doesn't shrink, so a map fills sooner under commits that rewrite much. The engine drops idle
  read transactions after each commit so that none holds freed pages (`Store::committed`); the pin holds one
  sync's worth by design.
- **A sync where a map is resized**, when another process committed meanwhile.
- **A sync and a record at each writable open**, and at the first open of a boot.
- **An open after a power loss loses the commits after the last sync**, as the grouped mode says. So does a
  process that died with commits unsynced, where the machine then restarts before the database is opened again,
  cleanly or not: today a clean restart keeps them.
- **Two refusals that are new**: an open in another mode than a live writer's, and an open that finds a restore it
  can't make.

## What was set aside

- **A marker of commits not yet synced, and the file refused when the machine restarted under it.** The first
  design. Its reader found it unsound as written: commits that pass no common function and are synced by nothing;
  a sync that failed, after which the same boot's open went on; a lock that doesn't exclude within a process on
  filesystems that emulate it; a marker stranded dirty by a process that never commits again. And sound, it put a
  sync on the path of the first commit after every pause, three syncs where an isolated commit has one, and
  refused files that were whole, since nearly every loss falls outside the few milliseconds a sync is writing.
- **The same marker kept dirty for a while after a sync**, to spare commits its sync. Every second it stays dirty
  is a loss after which a whole file is refused.
- **A restore beside other processes**, and so no refusal at all, as the second draft had it. A process with the
  database open takes its next transaction's number from the lock file and its trees from the meta page: after a
  restore it would build on the restored commit and give out pages its own readers still read.
- **A record that only its state ties to the file.** The second draft. A backup copied over the data file, or any
  file put there with later commits, would have had the old file's trees written into it at the next loss.
- **Writers of other modes told by one write of the record**, as the second draft had it. A grouped writer's
  record written after it, and the grouped writer gone, left them committing with no pin under a record naming a
  commit to restore: a commit a `syncEachCommit` writer had returned would have been undone.
- **A pin and a record kept by every writer, whatever its mode.** Sound, and a third sync on every
  `syncEachCommit` commit for a mix of modes that one refusal rules out.
- **A record written beside itself and renamed over.** Two syncs a record, a rename Windows refuses while another
  process has the file open, and a lock lost with the file it was on.
- **The checkpoint after a reset moved behind the reset's sync.** The pin would sit at the reset, and the load
  after it could use none of the pages the reset freed.
- **Page checksums.** This LMDB has none, and they would not see the commonest tear: a page left at its earlier,
  valid, content.
- **The record in LMDB's lock file.** Its first opener makes it again, and nothing syncs it.
- **The record written under LMDB's write lock.** LMDB has no way to try for it, and the thread that syncs would
  wait behind another process's open unit.

## Risks

- **It rests on one property of LMDB**, that a reader's pages are not written again. It is the property every
  read in elmdb rests on; the gates below hold it on a file, against the order LMDB's source gives its writes.
- **Writing a meta page.** A restore writes two pages of a file LMDB owns, by the layout `store/reach.rs` already
  reads, and keeps what it wrote over.
- **A file that grows under commits that rewrite much**, for good. Read in phase 1, with a bound.
- **Commits refused after a failed sync**, where today they are made. They were being made over pages that may
  not exist.
- **A dead process's pin.** LMDB keeps a dead reader until a check clears it, and elmdb clears them
  (`with_reader_recovery`). A pin cleared while its process's commits were unsynced leaves the record's commit
  unpinned until the next writer opens, which pins and syncs before it commits: no page is written in between.
- **A boot read wrongly.** Taken for the same boot, a loss opens the newest commit as today. Taken for another,
  a process's death costs it the commits after its last sync.
- **Programs that mix modes on one database at one time** are refused where they ran. apack opens its databases in
  one mode; whether anything else does is asked before phase 2.

## Phases

1. **The pin, the record and the one commit function.** Done when:
   - the power-loss replay (`../architecture/testing.md`, Power loss) makes, for the grouped mode, every file a loss
     could leave from the pages written since the last sync, each with the record that was durable then, and every
     one opens holding what the replay's model holds at the record's commit, and passes `check()`; and the same
     replay with the pin left out makes a file that doesn't, which is the test's proof that it can fail;
   - the replay's assertion that the grouped mode leaves at least one file that is no commit's is taken out, with
     the paragraph of `engine.md` that states the trade;
   - two processes, one holding a pin, the other committing values that spill and values on overflow pages: no
     page the pinned commit's meta page reaches changes in the file while the pin is held;
   - a restored database takes a workload over the transaction numbers the lost commits used, is opened again and
     passes `check()`; and an open that finds a restore beside another process, or read-only, is refused;
   - two commits forced between a pin's beginning and the read of its meta page: the page is not taken;
   - a model runs every order of a writer's commits, the syncing thread's steps, a second process's, a resized map
     and a loss, and holds that no commit is made while its process's pin is after the record's commit; and a test
     shows it failing with the old pin ended before the record is written;
   - a copy of the code in which each commit named above goes round the one function fails the repository's check;
   - a sync that fails: the commit after it is refused, another process's sync leaves the record `failed`, and the
     database opened again in the same boot opens at the record's commit;
   - a file put in place of the data file, with later commits and with earlier ones, is opened as it is at the
     next loss, the record not restored into it;
   - through the million-note load, and through a unit that rewrites a tenth of the notes each second for a
     minute, the file is no more than a tenth longer than without the pin, or the bound is set from the reading
     and stated.
2. **The modes.** `syncEachCommit` and `syncs: false` alone and after a grouped writer, a backup into a directory
   and `replaceWith`. Done when the replay holds each, an open in another mode than a live writer's is refused and
   one after it has gone is not, and a `syncEachCommit` writer is shown to sync no more than it does today.
3. **The boot on each platform, and the report.** Done when an open after a loss says what it opened at in the
   binding and the CLI; the comparison of boots on Windows is held as a function of its own over a sleep, a
   stepped clock and a clock that starts again; and each platform's boot equals itself across two reads.
