# A power loss that tears no file

Status: designed, 2026-11-14, and not built. It is the answer to the one defect of the review of `ed20979` that was
left for a design (`../roadmap.md`, change 1): in the grouped mode a power loss can leave a file that is no commit's,
which reads wrongly or ends the process that reads it. This is the fifth draft. One reader read the four before it
against the store and LMDB's source: the first, a marker of commits not yet synced, did not hold; the others were
this design, and what each reading changed is under What was set aside. The fourth was read as a protocol to model,
and found ready once three of its steps were restated, which this draft does. Its first phase is built
(`store/commits.rs`).

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

## The readings

How often a file is torn, and what the mode that can't be costs the program elmdb was written for:

- A tear needs a meta page on disk without the pages it names. Between syncs the OS writes little of a file that is
  synced each second, so it is the loss that falls inside a sync's own writing, a few milliseconds, that tears. A
  loss at any other moment after a commit leaves the last synced commit whole on disk and the commits after it
  gone. This is the second reader's reasoning from LMDB's order of writes, not a measurement.
- apack wrote a streamed reply's text whole on every chunk, 181 commits for a reply of 400 words
  (`../evidence/next-phase-10.md`), until its commit `d9102d235b` of 2026-11-09. Since then it appends the text on
  a clock of 250 ms and writes it whole once at the end: about 40 commits for such a reply streamed over ten
  seconds, counted by a test in apack's pull request 483 (2026-11-14). At the 0.03 ms a grouped commit took and
  the 8.0 ms a `syncEachCommit` one took on an Apple M1 Pro (2026-11-12, `../architecture/engine.md`, Durability),
  that reply's commits are about 1 ms in the one mode and a third of a second in the other, a thirtieth of the
  time it streams. Neither was measured in apack. apack opens every database it keeps in the grouped mode, asks
  for no durable write, opens temporary ones with `syncs: false`, and opens its databases read-only beside the
  running app from its CLI and its backup (apack's `packages/apack-ears/src/lmdb/database.ts` and
  `packages/apack-host`, read 2026-11-14).

What they say: `syncEachCommit` as the default needs no new code, and by the count as it now stands it may well
serve apack: the fourth draft set it aside on the count of 181, which was out of date. Which of the two to build
is open again, and is read in apack before more of this design is built. And a restore will run far more often
than a tear happens, at every loss within a second of a commit: it is a path that runs, and must be right.

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
meta page that names them, which the second commit after it writes over. And with no reader at all LMDB keeps the
newest commit's pages from the next two transactions, so a commit on disk stays whole until a third follows it.

## The design

**The last synced commit is kept: its pages by a read transaction, its meta page by a file beside the data.** After a
power loss the database is opened at that commit: what was not synced is gone, as the grouped mode says it may be,
and what was is whole.

| What | Holds |
|---|---|
| The pin | a read transaction a process writing in the grouped mode holds while it has commits not yet synced, at the commit the record names or an earlier one |
| `synced`, a file beside the data file | the record: the body of the meta page of the last commit known to be on disk, a checksum of the two root pages it names, the boot it was written in, the mode the database is written in, a state, a notice, a count of the records written, and a checksum of them all |
| `synced.restored` | the two meta pages a restore wrote over, as they were, written once |
| Lock files, one for each purpose | nothing: that a process has the database open, that one writes it in a mode, and that one is writing the record |

- **The record is two slots written in turn, in place**, each in a block of its own with its own checksum, the one
  of the higher count that reads being the record: one sync for each record, and no file made or renamed. Where a
  filesystem overwrites a file in place a full disk doesn't stop one. The file is made, and its directory synced,
  by the first writable open.
- **A lock is a file of its own, never written**, locked whole. A lock on a range of a file is the process's on
  unix and goes when it closes any descriptor of that file, and on Windows keeps other processes from reading the
  range. LMDB's lock file is none of them, and a test holds that nothing but LMDB opens it.
- **A meta page is taken whole.** A sync begins a read transaction and reads its commit's meta page; it takes it
  only if the file's other meta page is of no later commit, and otherwise begins again. A commit two after it
  writes that page field by field, and a read beside it could take one commit's free tree with another's rows.
- **A sync moves the pin, in this order**: a read transaction is begun and its meta page taken; the record is
  read, a process's memory of it being no more than the least it can be; the data file is synced; the record is
  written and synced; the transaction that was the pin is ended. If the process has no commit after the one the
  record now names, the new transaction is ended too, in one step with any write of the process that is
  beginning: a process that owes nothing holds no pin, so it holds back no page another process frees.
- **A process writes only while it holds a pin at or before the record's commit, taken before its write
  transaction begins.** Not at its commit: a transaction takes pages as it runs, and one too large for its memory
  writes them to the file before it commits, so with no pin and two commits after the record's it could write
  over the record's commit before its own pin was there. A write that finds no pin pins the newest commit, and if
  that is later than the record's, another process having committed since, syncs and writes the record before it
  begins. A writable open does the same before its first write, as does a process whose pin was ended to let the
  map be resized, by it or by another process (LMDB changes a map's size only while the process has no
  transaction), before the unit that filled the map is run again. The pin's own beginning can meet a map another
  process grew, and takes it as any transaction does. One process writing pays nothing by this rule; of two, one
  pays a sync at its first write after a pause while the other is writing, and pays it outside LMDB's write lock.
- **A sync is due by what the pin holds back as well as by time**: once the file's last page is further past what
  it was at the pin than a bound, the greater of a fixed size and a fraction of the file, whichever process's
  commits moved it. Only a process that holds a pin asks: one that holds none holds nothing back. A file grows
  by that bound and by what is committed while a sync runs, and no further.
- **A record is written by one process at a time and its commit never goes back.** Its writer holds the record's
  lock between processes and a mutex within one, and writes the commit of its pin only where that is later than
  the record's: of two processes syncing at once, the one whose pin is earlier writes nothing, or a commit a
  caller was told is on disk could be undone at the next loss. The boot, the state and the notice are written
  again at the record's own commit.
- **The record's lock is never held while LMDB's write lock is taken.** An open's own commits are made after it
  lets the record's lock go, and a write's sync is made before its transaction begins.
- **Every commit of a write transaction goes through one function**, which is where the rule is held: the open's
  own commits, the vector graphs' databases made at open and the checkpoint after a reset among them
  (`store/mod.rs`, `with_graphs`, `recycle_reset_pages`), which today commit beside it, the first of them synced
  by nothing. A repository check holds that no other commit of a write transaction is in the store or the engine,
  as `check-sync` holds the locks.
- **A reset keeps its order**: its commit, the checkpoint that lets a load use the pages it freed, then the pin at
  the checkpoint's commit, the sync and the record.
- **The boot** is an identity the machine loses when it stops and no clock makes: `/proc/sys/kernel/random/boot_id`
  on Linux, and where a sandbox hides it a token in a filesystem kept in memory; `kern.bootsessionuuid` on Apple's
  systems; on Windows a random value in a registry key made volatile, which is kept in memory only. One that is
  lost early, at a logoff, reads as another boot; none is kept across a cold start. Where none can be had, a
  record is of no boot, which is another boot than any.
- **A database another process has open is in this boot.** Every open, a read-only one too, holds the lock that
  says so, and no lock outlasts a restart. So an open beside a live process never takes its unsynced commits for a
  loss, whatever the boot reads as. An open takes that lock while it holds the lock every open decides under, and
  after it has decided and made any restore, and a refused open lets it go first: a process that started after a
  loss and has not yet restored is then never taken for one that holds what was written. Where an open can't make
  the lock files, on a filesystem it can't write, it is refused as it is where it can't make LMDB's.
- **The state** is `kept`, or `failed` once a sync fails.
- **The notice** says a restore was made, and the sequence it opened at. Every open reports it until a caller says
  it has been read.

**A database is written in one mode at a time.** The grouped mode keeps the pin and the record. A process that syncs
each commit, or none (`syncEachCommit`, `syncs: false`), keeps neither, and the record says which: `ordered`, under
which LMDB's own order of writes leaves the file some commit's after any loss, or `unsynced`, under which nothing is
known of it, as nothing is today. `syncEachCommit` rests on LMDB's order alone, as it does now: a caller who picks
it picks it to depend on nothing this design adds. Every writable open holds the lock of its mode, and an open in
another mode than a live writer's is refused (`MODE_MISMATCH`, a new code), naming both. The first writable open in
a mode makes any restore there is to make, then syncs the data file, since a writer of another mode may have died
with commits unsynced, and then writes the record for its mode, synced, before its first write.

**What an open does**, before LMDB opens the file, holding the record's lock:

- *No record*: a database from before records, or a copy a backup made. It is opened as it is, and a writable open
  writes its record.
- *A record the file doesn't bear out*: the record names a later commit than the file's newer meta page; or the
  same commit and another meta body; or root pages whose checksum isn't the file's. A file was put where another
  was. Under a `kept` record it is opened as it is and the open's report says so; under a `failed` one it is
  refused (`CORRUPTED`), the mismatch being what a failed sync leaves.
- *An `ordered` or `unsynced` record*: opened as it is.
- *A `kept` record, and another process has the database open, or the record is of this boot*: a process died, or
  closed, or is there. The OS holds what was written, so the newest commit is whole, and the database is opened
  at it.
- *A `kept` record of another boot whose commit is the file's newest*: nothing was lost. A writable open writes
  the record again as of this boot, under the lock it decides under, and a read-only one leaves that to the next
  writable open.
- *A `kept` record of another boot whose commit is before the file's newest, and no other process*: the machine
  stopped with commits made after the last sync, and what of them reached the disk is not known, so the record's
  commit is restored.
- *A `failed` record*: the record's commit is restored.
- *A record neither of whose slots reads* is refused (`CORRUPTED`): each is written whole and one always stands.

**A restore** is the one write to a file LMDB owns. It is made by an open that holds the database alone, no other
process having it open, read-only or not: another process would go on from the commits being undone, its readers on
pages about to be given out again. A read-only open makes it where it can write the file, unless its caller forbids
any write. An open that finds a restore to make and can't make it is refused (`RESTORE_PENDING`, a new code), saying
to close the other processes, or to open the database writable once. The restore saves the file's two meta pages
beside it if none are saved, writes the record's meta body to both, from the map's size on as LMDB writes one, syncs
the file, and writes the record again as of this boot, `kept`, with the notice: stopped at any step, the next open
makes it again from the same record. LMDB then opens at the record's commit.

**A sync that fails** writes the record `failed`. From then the process refuses every commit, as it refuses every
wait today, and keeps its pin until it ends; a process that finds the record `failed` at its own sync does the same,
as does one whose sync before a write fails: only a restore writes over it. If the record can't be written either,
the database goes on as it does today, which the error says.

**What else writes the data file.** A backup made into a directory, and `replaceWith`, write the record for the data
they put there or remove it. `replaceWith` opens its source as an open does, record and all.

**What this doesn't reach.** An elmdb from before this design keeps no pin and reads no record: a database it wrote
to under a `kept` record that then lost power is restored to a commit whose pages it may have written over, which
the checksum of the root pages catches where a root was written and not below. A later copy of the same database
put in place of the data file, by a tool that copies files, still has the record's root pages where they were until
they are used again, and is taken back to the record's commit at the next loss. And this boot means the OS holds
what was written: a volume lost and mounted again within one boot is outside what `filesystems.md` supports. The
first is the cost the format's rule accepts until a release has users (`../architecture/format.md`, Versions).

## What it costs

- **Nothing on the path of a write of the one process writing.** Of two processes writing in turn, the one that
  begins while the other has commits unsynced pays a sync before its first.
- **A sync of the record after each sync of the data**, on the thread that syncs: two syncs where there was one. A
  `durable` write waits for both, about 8 ms where it was 4.0 on an Apple M1 Pro by `engine.md`'s reading of
  2026-11-12, which is what `syncEachCommit` costs there.
- **A reader's place in the lock file** for a process with commits unsynced.
- **Pages freed since the pin wait for the next record before they are used again**, and a file's length doesn't
  shrink: it grows by the bound above and by what is committed while a sync runs. The engine drops idle read
  transactions after each commit so that none holds freed pages (`Store::committed`); the pin holds a bound's
  worth by design.
- **A sync where a map is resized**, when commits are unsynced or another process committed meanwhile.
- **A sync and a record at each writable open** that finds a later commit than the record's, and a record at the
  first open of a boot.
- **An open after a power loss loses the commits after the last sync**, as the grouped mode says. So does a
  process that died with commits unsynced where the machine then restarts before the database is opened again,
  cleanly or not: today a clean restart keeps them.
- **Two refusals that are new**: an open in another mode than a live writer's, and an open that finds a restore it
  can't make.

## What was set aside

- **`syncEachCommit` as the default, and the grouped mode asked for**, set aside in the fourth draft and open
  again: no new code, and the simplest thing that is sound. It was set aside on a count of apack's commits that
  was out of date (The readings).
- **A marker of commits not yet synced, and the file refused when the machine restarted under it.** The first
  design. Its reader found it unsound as written: commits that pass no common function and are synced by nothing;
  a sync that failed, after which the same boot's open went on; a lock that doesn't exclude within a process on
  filesystems that emulate it; a marker stranded dirty by a process that never commits again. And sound, it put a
  sync on the path of the first commit after every pause, three syncs where an isolated commit has one, and
  refused files that were whole.
- **The same marker kept dirty for a while after a sync**, to spare commits its sync. Every second it stays dirty
  is a loss after which a whole file is refused.
- **One mechanism for every writer that syncs**, `syncEachCommit` made the grouped mode with every commit waiting
  for its record. It would free the modes to mix and take a state away, and it would put the mode a caller picks
  to depend on LMDB's order alone on the record, the boot, a restore and the pin as well. If mixing is ever
  wanted, `syncEachCommit` keeps LMDB's order and adds the pin and a record for each commit, at a third sync.
- **A restore beside other processes**, as the second draft had it. A process with the database open takes its
  next transaction's number from the lock file and its trees from the meta page: after a restore it would build
  on the restored commit and give out pages its own readers still read.
- **A lock held by writers only**, as the third draft had it. A restore would not have seen a read-only process;
  and where the boot can't be read, an open beside the running program would have found a restore it couldn't
  make whenever the program had commits unsynced, which is how apack's CLI and its backup open a database.
- **A pin taken at a process's first commit**, as the fourth draft had it. A unit run again after its map was
  resized, the large one, writes pages to the file before it commits; two commits after the record's and no pin,
  it could write over the record's commit. The pin is taken before the transaction begins.
- **A record its writer's pin alone decides**, as the fourth draft had it. Of two processes syncing at once the
  one pinned earlier would have written the record back, and undone a commit a caller was told was on disk.
- **A pin held from one sync to the next**, as the third draft had it. A process that stopped writing would have
  held back every page another process freed, for as long as it stayed open.
- **The other meta page required to be earlier**, as the third draft had it. A new database's two are the same,
  as are a restored one's, and its first sync would never have taken one.
- **A record that only its state ties to the file.** The second draft. A backup copied over the data file, or any
  file put there with later commits, would have had the old file's trees written into it at the next loss.
- **Writers of other modes told by one write of the record**, as the second draft had it. A grouped writer's
  record written after it, and the grouped writer gone, left them committing with no pin under a record naming a
  commit to restore: a commit a `syncEachCommit` writer had returned would have been undone.
- **A record written beside itself and renamed over.** Two syncs a record, a rename Windows refuses while another
  process has the file open, and a lock lost with the file it was on.
- **Locks on ranges of the record's file.** See the lock above.
- **The boot told from clocks**, the wall time less the uptime. A clock that starts again at each power loss, a
  dead battery's, reads the same boot after a loss, which is the one wrong answer that opens a torn file.
- **The epoch moved by a restore**, so that no handle takes a sequence of the commits that went for a later one.
  The epoch speaks to handles other processes hold, and a restore has none; the notice is what a caller reads.
- **The checkpoint after a reset moved behind the reset's sync.** The pin would sit at the reset, and the load
  after it could use none of the pages the reset freed.
- **LMDB changed** to keep a commit's meta page in memory until its group is synced. Its lock file is made again
  by the first process to open it, so the last process's death would lose commits that had returned, and every
  reader's way to its commit would change, in the one dependency the repository holds to a single copy and reads
  each week for upstream's changes. A smaller change, a commit below which LMDB frees no page, would stand in for
  the pin, and is worth it only if the pin proves troublesome.
- **A log of batches beside LMDB**, replayed over the restored commit. A second format to keep durable, a replay
  that must make the same ids, times and codes again, and it needs the pin and the restore all the same.
- **Page checksums.** This LMDB has none, and they would not see the commonest tear: a page left at its earlier,
  valid, content.
- **The record in LMDB's lock file.** Its first opener makes it again, and nothing syncs it.
- **The record written under LMDB's write lock.** LMDB has no way to try for it, and the thread that syncs would
  wait behind another process's open unit.

## Risks

- **It rests on one property of LMDB**, that a reader's pages are not written again. It is the property every
  read in elmdb rests on; the gates below hold it on a file, against the order LMDB's source gives its writes, and
  on the real engine with two processes.
- **The restore runs at every loss within a second of a commit**, where a tear needs a loss within a sync. It
  writes two pages of a file LMDB owns, by the layout `store/reach.rs` already reads, and keeps what it wrote
  over.
- **Every hole three readings found was in the order of steps between processes**, not in a line of code. The
  model below is what holds that order, and it is built before the code it models.
- **Commits refused after a failed sync**, where today they are made. They were being made over pages that may
  not exist.
- **A dead process's pin.** LMDB keeps a dead reader until a check clears it, and elmdb clears them
  (`with_reader_recovery`). A pin cleared while its process's commits were unsynced leaves the record's commit
  unpinned until the next write, which pins and syncs before its transaction begins: no page is written in
  between.
- **A boot read wrongly as another** costs a process's death the commits after its last sync, and no refusal.
  Read wrongly as the same it opens what a loss left, which is today's defect; the identities above are the ones
  that can't be read so.

## Phases

1. **One function for every commit.** Built (`store/commits.rs`, `cargo xtask check-commits`): the open's commits,
   the graphs' and the checkpoint's go through it, and the graphs' commit is synced.
2. **The model**, built before the code it models, every hole four readings found having been in the order of
   steps between processes.
   - *What it holds.* The file's pages, its two meta pages, each written in two steps, its body and then its
     commit, and the record's two slots, each as the OS holds it and as the disk does. LMDB's lock file, which is
     lost with its last holder: the newest commit, the readers, the write lock. LMDB's rule: each commit's pages,
     the pages each commit freed, and a transaction taking only what was freed before the least of the live
     readers and the commit before its own. For each process: whether it lives, its mode, the locks it holds, its
     old and new pin, the record's commit as it remembers it, whether it has a commit after the record's, whether
     its sync failed, and where it is in a write, a sync, an open, a restore or a resize. The boot and what its
     identity reads as, right, another or none, never wrongly the same. The commits that returned and those a
     caller was told were on disk. And, for the invariants alone, each commit's pages as they were when a record
     naming it became durable, and the rows at each commit.
   - *Its steps.* A write: the pin made sure of, the write lock, pages taken at any time and some written to the
     OS, the commit's pages, the meta page's body, its commit, the lock file's newest commit, the lock let go, the
     return; an abort anywhere; a full map, with the pins ended, the resize and the unit again. A sync, step by
     step as above, a failing one putting some of the OS's pages back to the disk's. An open, with each step of a
     restore, read-only and writable, in each mode; a close. A process's death at any step, its locks gone and its
     reader left until some process's check. A loss: every process dead, each page, meta page and slot not synced
     independently as it was or as it is, the lock file gone, the boot another.
   - *What must hold.* After any loss an open ends at a commit whose pages are as they were synced, whose rows are
     that commit's, and which is no earlier than any commit a caller was told was on disk. After deaths alone it
     ends at the newest commit. No page a process following the rules writes is in the durable record's commit's
     trees. The record's commit never goes back, and only a restore writes over `failed`. No step of a restore is
     made while another process has the database open. With no further death or loss every write, sync, open and
     restore ends. No writer without a pin writes under a durable `kept` record, and a commit a `syncEachCommit`
     writer returned is never undone.
   - *Done when* it holds those within three processes, eight commits, ten pages and two losses, and shows each of
     these failing: the pin taken at the commit, with pages written before it; the record let go back; the old pin
     ended before the record is written; the lock that says a database is open taken outside the open's; the
     other meta page required earlier; the other meta page not read; a restore beside a live process; the record
     written before the data is synced; a mode's record written before the data is synced; the new pin ended
     while a write begins; `failed` written over by a sync. A process that follows none of the rules is a run of
     its own, which shows what this design doesn't reach.
3. **The record, the boot, the locks and the pin.** Done when:
   - the power-loss replay (`../architecture/testing.md`, Power loss) makes, for the grouped mode, every file a loss
     could leave from the pages written since the last sync, each with the record that was durable then, and every
     one opens holding what the replay's model holds at the record's commit, and passes `check()`; and the same
     replay with the pin left out makes a file that doesn't;
   - the replay's assertion that the grouped mode leaves at least one file that is no commit's is taken out, with
     the paragraph of `engine.md` that states the trade;
   - two processes, one holding a pin, the other committing values that spill and values on overflow pages: no
     page the pinned commit's meta page reaches changes in the file while the pin is held, and in the same run
     one does once the pin is ended;
   - a process is ended at each step of a sync and of a restore, and the next open ends where the model says;
   - a restored database takes a workload over the transaction numbers the lost commits used, is opened again and
     passes `check()`; an open that finds a restore beside another process is refused; and a read-only open beside
     a writer with commits unsynced, where no boot can be read, opens;
   - a new database, and a restored one, whose two meta pages are of one commit, syncs;
   - one commit forced between a pin's beginning and the read of its meta page: the page is not taken;
   - a sync that fails: the commit after it is refused, another process's sync leaves the record `failed`, and the
     database opened again in the same boot opens at the record's commit;
   - a process that stops writing while another writes for a minute: the file is no longer than the bound past
     what the second alone makes it; and through the million-note load, and a unit that rewrites a tenth of the
     notes each second for a minute, it is within the bound, which is set before the reading;
   - a unit run again after its map was resized, two commits after the record's, with a loss replayed over the
     pages it wrote before its commit; an open beside a write's first sync; and a `durable` write beside a second
     process's sync, which the next loss doesn't undo.
   Read beside today's, and gates of nothing: a `durable` write, a commit of one process, and the first write of a
   second while the first is writing.
4. **The modes.** `syncEachCommit` and `syncs: false` alone and after a grouped writer that died with commits
   unsynced, a backup into a directory and `replaceWith`. Done when the replay holds each, an open in another mode
   than a live writer's is refused and one after it has gone is not, and a `syncEachCommit` writer is shown to
   sync no more than it does today.
5. **The boot on each platform, the notice, and a filesystem that loses what wasn't synced.** Done when an open
   after a loss says what it opened at in the binding and the CLI until it is told it was read; each platform's
   identity is shown gone after a machine is stopped hard and started, and what a fast startup does to Windows' is
   written down, by hand on a machine of each; and the engine runs each night on Linux over a filesystem that
   drops or tears what was not synced, and opens at the record's commit.
