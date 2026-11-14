//! A model of how the grouped mode keeps its last synced commit (`docs/roadmap/synced-commit.md`), built before the
//! code it models: every hole the design's readings found was in the order of steps between processes, and this
//! runs every such order within its bounds.
//!
//! It holds a file as the OS has it and as the disk has it, LMDB's lock file and its rule for giving out pages, and
//! processes that write, sync, die and open, one step at a time in every order, with a loss of power at any point.
//! After each loss it opens the database as the design says and holds the result to what was promised. Each rule of
//! the design can be turned off (`Rules`), and a test shows the model failing without it: a rule whose absence the
//! model can't see is a rule the model doesn't hold.
//!
//! What it leaves out: the modes other than the grouped one, a sync that fails, a map resized, an open read-only,
//! and a restore stopped part way. They are the design's later phases, and are added here before each is built.

use std::collections::HashSet;

const PROCESSES: usize = 2;
const PAGES: usize = 6;
/// Commits are numbered from 0, the one a new database is at, to one less than this
const COMMITS: usize = 6;
/// No commit, no page's version, no process
const NONE: u8 = u8::MAX;
/// A page no commit has used
const FRESH: u8 = u8::MAX - 1;

/// One of the file's two meta pages: the commit whose trees it names, and the commit it says it is. A commit writes
/// the first before the second, so a page read beside one may name one commit's trees under another's number
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
struct Meta {
    body: u8,
    id: u8,
}

/// The record beside the data file: the commit it names, the commit whose trees its copy of the meta page names,
/// and the boot it was written in. A commit of `NONE` is no record
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
struct Record {
    commit: u8,
    body: u8,
    boot: u8,
}

/// What LMDB holds at a commit: each page's version in the commit's trees (`NONE` where it has none), and each
/// page's place among the free ones, the commit that freed it, `FRESH`, or `NONE` for a page in use
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
struct Snapshot {
    tree: [u8; PAGES],
    free: [u8; PAGES],
}

/// Where a process is in a write, or in a sync: it does both at once, a thread at each
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum At {
    Idle,
    /// A write: its pin made sure of, a sync waited for where the pin needs one, the write lock, its pages chosen,
    /// some written before the commit, the commit's pages, the meta page's trees, its number, and LMDB's lock file
    /// told
    WritePin,
    WriteAwaitsSync,
    WriteLock,
    WriteBegin,
    WritePages,
    WriteBody,
    WriteId,
    WriteTable,
    /// A sync: a read transaction begun, its meta page read, the read held to its rule, the data synced, the record
    /// written, the record synced, and the pins moved
    SyncPin,
    SyncSlot,
    SyncCheck,
    SyncData,
    SyncRecord,
    SyncRecordSynced,
    SyncUnpin,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
struct Process {
    alive: bool,
    /// Its read transactions in LMDB's lock file: the pin, and the one a sync is moving it to. A dead process's stay
    /// until some process's check clears them
    pins: [u8; 2],
    /// The record's commit as it last knew it to be on disk
    remembered: u8,
    /// Its last commit that returned
    last: u8,
    write: At,
    sync: At,
    /// A write's commit, the page it replaces and the page it writes
    writing: [u8; 3],
    /// A sync's reading of its meta page: the commit whose trees it names, or `NONE` when it wasn't the pin's
    captured: u8,
}

/// The rules of the design, each of which a test turns off
#[derive(Clone, Copy, Debug)]
struct Rules {
    /// A write's pin is taken before its transaction begins, not at its commit
    pin_before_write: bool,
    /// A record is written only for a later commit than it names
    record_never_back: bool,
    /// The pin a sync moves from is ended after the record is synced, not before
    old_pin_until_record: bool,
    /// The data is synced before the record is written
    data_before_record: bool,
    /// A process relies on a record only once it has synced it, never on one another process wrote
    record_synced_by_reader: bool,
    /// A sync that ends keeps a pin for a write of its process that has begun
    pin_kept_for_a_write: bool,
    /// How a sync holds its reading of a meta page to be of one commit
    slot: Slot,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Slot {
    /// LMDB's lock file still names the pin's commit as its newest after the page is read
    NewestUnchanged,
    /// The other meta page is of no later commit
    OtherNotLater,
    /// The other meta page is of an earlier commit
    OtherEarlier,
    /// Only that the page read says it is the pin's commit
    Unchecked,
}

const DESIGN: Rules = Rules {
    pin_before_write: true,
    record_never_back: true,
    old_pin_until_record: true,
    data_before_record: true,
    record_synced_by_reader: true,
    pin_kept_for_a_write: true,
    slot: Slot::NewestUnchanged,
};

/// How far a run goes
#[derive(Clone, Copy, Debug)]
struct Bounds {
    deaths: u8,
    losses: u8,
    opens: u8,
    /// Whether a write may write its pages to the file before it commits, as one too large for its memory does
    spills: bool,
}

#[derive(Clone, PartialEq, Eq, Hash, Debug)]
struct World {
    snapshots: [Snapshot; COMMITS],
    os_pages: [u8; PAGES],
    disk_pages: [u8; PAGES],
    os_metas: [Meta; 2],
    disk_metas: [Meta; 2],
    os_record: Record,
    disk_record: Record,
    /// LMDB's lock file: its newest commit (`NONE` when no process has the database open), and who holds the write
    /// lock
    newest: u8,
    writer: u8,
    processes: [Process; PROCESSES],
    boot: u8,
    /// The newest commit that returned, and the newest a caller was told is on disk
    returned: u8,
    on_disk: u8,
    /// Whether power was lost and no process has opened the database since
    lost: bool,
    deaths: u8,
    losses: u8,
    opens: u8,
}

const NO_PROCESS: Process = Process {
    alive: false,
    pins: [NONE; 2],
    remembered: NONE,
    last: NONE,
    write: At::Idle,
    sync: At::Idle,
    writing: [NONE; 3],
    captured: NONE,
};

impl World {
    /// A new database, synced, with no record and no process
    fn new() -> World {
        let mut first = Snapshot { tree: [NONE; PAGES], free: [FRESH; PAGES] };
        for page in 0..2 {
            first.tree[page] = 0;
            first.free[page] = NONE;
        }
        let mut pages = [NONE; PAGES];
        pages[..2].fill(0);
        let none = Record { commit: NONE, body: NONE, boot: 0 };
        World {
            snapshots: [first; COMMITS],
            os_pages: pages,
            disk_pages: pages,
            os_metas: [Meta { body: 0, id: 0 }; 2],
            disk_metas: [Meta { body: 0, id: 0 }; 2],
            os_record: none,
            disk_record: none,
            newest: NONE,
            writer: NONE,
            processes: [NO_PROCESS; PROCESSES],
            boot: 0,
            returned: 0,
            on_disk: 0,
            lost: false,
            deaths: 0,
            losses: 0,
            opens: 0,
        }
    }

    /// The earliest commit a read transaction is at, a dead process's among them until they are cleared
    fn oldest_reader(&self) -> u8 {
        self.processes.iter().flat_map(|process| process.pins).filter(|pin| *pin != NONE).min().unwrap_or(NONE)
    }

    /// The meta page LMDB opens a file by: the one of the later commit, the first of two of one commit
    fn newer_meta(metas: &[Meta; 2]) -> Meta {
        if metas[1].id > metas[0].id { metas[1] } else { metas[0] }
    }
}

/// What the model found wrong, with the steps that led to it
type Broken = (&'static str, Vec<String>);

struct Search {
    rules: Rules,
    bounds: Bounds,
    seen: HashSet<World>,
    path: Vec<String>,
}

impl Search {
    fn broken(&self, what: &'static str) -> Broken {
        (what, self.path.clone())
    }

    /// Every world one step from `world`, each with the step's name, or what the step broke
    fn steps(&self, world: &World) -> Result<Vec<(String, World)>, Broken> {
        let mut next = Vec::new();
        let any_alive = world.processes.iter().any(|process| process.alive);
        for at in 0..PROCESSES {
            let process = &world.processes[at];
            if !process.alive {
                if world.opens < self.bounds.opens {
                    next.push((format!("{at} opens"), self.open(world, at)?));
                }
                continue;
            }
            self.advance(world, at, &mut next)?;
            if world.deaths < self.bounds.deaths {
                let mut died = world.clone();
                died.deaths += 1;
                died.processes[at] = Process { alive: false, pins: process.pins, ..NO_PROCESS };
                if died.writer == at as u8 {
                    died.writer = NONE;
                }
                next.push((format!("{at} dies"), died));
            }
        }
        let dead_pins = world.processes.iter().any(|process| !process.alive && process.pins != [NONE; 2]);
        if any_alive && dead_pins {
            let mut cleared = world.clone();
            for process in cleared.processes.iter_mut().filter(|process| !process.alive) {
                process.pins = [NONE; 2];
            }
            next.push(("the dead's readers are cleared".to_owned(), cleared));
        }
        if any_alive && world.losses < self.bounds.losses {
            self.lose(world, &mut next);
        }
        Ok(next)
    }

    /// A live process's next steps: its write's and its sync's, each of them where it has a choice
    fn advance(&self, world: &World, at: usize, next: &mut Vec<(String, World)>) -> Result<(), Broken> {
        let process = world.processes[at];
        if process.write == At::WriteBegin {
            self.begin(world, at, next)?;
        }
        let rules = self.rules;
        let mut step = |name: &str, change: &dyn Fn(&mut World, &mut Process) -> Result<(), &'static str>| {
            let mut after = world.clone();
            let mut moved = process;
            change(&mut after, &mut moved)?;
            after.processes[at] = moved;
            next.push((format!("{at} {name}"), after));
            Ok::<(), &'static str>(())
        };
        let owes = |p: &Process| p.last != NONE && (p.remembered == NONE || p.last > p.remembered);
        let write = match process.write {
            At::Idle if world.newest != NONE && usize::from(world.newest) + 1 < COMMITS => {
                step("begins a write", &|_, p| {
                    p.write = if rules.pin_before_write { At::WritePin } else { At::WriteLock };
                    Ok(())
                })
            }
            At::WritePin => step("makes sure of its pin", &|w, p| {
                let relied = if rules.record_synced_by_reader { p.remembered } else { w.os_record.commit };
                if p.pins[0] != NONE {
                    p.write = At::WriteLock;
                } else if relied != NONE && w.newest <= relied {
                    p.pins[0] = w.newest;
                    p.remembered = relied;
                    p.write = At::WriteLock;
                } else {
                    // The newest commit is later than the record it can rely on: a sync and a record first
                    p.write = At::WriteAwaitsSync;
                }
                Ok(())
            }),
            // The sync it waited for has ended, and left it the pin
            At::WriteAwaitsSync if process.sync == At::Idle && process.pins[0] != NONE => {
                step("has its pin from the sync", &|_, p| {
                    p.write = At::WriteLock;
                    Ok(())
                })
            }
            At::WriteLock if world.writer == NONE => step("takes the write lock", &|w, p| {
                w.writer = at as u8;
                p.write = At::WriteBegin;
                Ok(())
            }),
            At::WritePages => step("writes its commit's pages", &|w, p| {
                if !rules.pin_before_write && p.pins[0] == NONE {
                    p.pins[0] = w.newest;
                }
                write_page(w, p.writing[2], p.writing[0])?;
                p.write = At::WriteBody;
                Ok(())
            }),
            At::WriteBody => step("writes the meta page's trees", &|w, p| {
                // From here the page names this commit's trees, whatever number it is under: what the commit holds
                // is LMDB's from this step
                let [commit, replaced, written] = p.writing;
                let mut snapshot = w.snapshots[usize::from(w.newest)];
                snapshot.tree[usize::from(replaced)] = NONE;
                snapshot.free[usize::from(replaced)] = commit;
                snapshot.tree[usize::from(written)] = commit;
                snapshot.free[usize::from(written)] = NONE;
                w.snapshots[usize::from(commit)] = snapshot;
                w.os_metas[usize::from(commit & 1)].body = commit;
                p.write = At::WriteId;
                Ok(())
            }),
            At::WriteId => step("writes the meta page's commit", &|w, p| {
                w.os_metas[usize::from(p.writing[0] & 1)].id = p.writing[0];
                p.write = At::WriteTable;
                Ok(())
            }),
            At::WriteTable => step("returns from its commit", &|w, p| {
                let commit = p.writing[0];
                w.newest = commit;
                w.writer = NONE;
                w.returned = commit;
                p.last = commit;
                p.writing = [NONE; 3];
                p.write = At::Idle;
                Ok(())
            }),
            _ => Ok(()),
        };
        write.map_err(|what| self.broken(what))?;
        let sync = match process.sync {
            // Due once the process has a commit after the record's, or a write of it waits for one
            At::Idle if owes(&process) || process.write == At::WriteAwaitsSync && process.pins[0] == NONE => {
                step("begins a sync", &|_, p| {
                    p.sync = At::SyncPin;
                    Ok(())
                })
            }
            At::SyncPin => step("begins a read transaction", &|w, p| {
                p.pins[1] = w.newest;
                p.sync = At::SyncSlot;
                Ok(())
            }),
            At::SyncSlot => step("reads its commit's meta page", &|w, p| {
                let read = w.os_metas[usize::from(p.pins[1] & 1)];
                p.captured = if read.id == p.pins[1] { read.body } else { NONE };
                p.sync = At::SyncCheck;
                Ok(())
            }),
            At::SyncCheck => step("holds the reading to one commit", &|w, p| {
                let pin = p.pins[1];
                let other = w.os_metas[usize::from((pin & 1) ^ 1)].id;
                let whole = p.captured != NONE
                    && match rules.slot {
                        Slot::NewestUnchanged => w.newest == pin,
                        Slot::OtherNotLater => other <= pin,
                        Slot::OtherEarlier => other < pin,
                        Slot::Unchecked => true,
                    };
                if whole {
                    if !rules.old_pin_until_record {
                        p.pins[0] = NONE;
                    }
                    p.sync = if rules.data_before_record { At::SyncData } else { At::SyncRecord };
                    return Ok(());
                }
                // Nothing is being written and LMDB's newest commit is the pin's still: the same reading again, for
                // as long as no process commits
                if w.writer == NONE && w.newest == pin {
                    return Err("a sync that reads its meta page again and again, and never takes it");
                }
                p.pins[1] = NONE;
                p.sync = At::SyncPin;
                Ok(())
            }),
            At::SyncData => step("syncs the data", &|w, p| {
                w.disk_pages = w.os_pages;
                w.disk_metas = w.os_metas;
                p.sync = if rules.data_before_record { At::SyncRecord } else { At::SyncUnpin };
                Ok(())
            }),
            At::SyncRecord => step("writes the record", &|w, p| {
                let later = w.os_record.commit == NONE || p.pins[1] > w.os_record.commit;
                if later || !rules.record_never_back {
                    w.os_record = Record { commit: p.pins[1], body: p.captured, boot: w.boot };
                }
                p.sync = At::SyncRecordSynced;
                Ok(())
            }),
            At::SyncRecordSynced => step("syncs the record", &|w, p| {
                w.disk_record = w.os_record;
                p.remembered = w.disk_record.commit;
                if rules.data_before_record {
                    // The data was synced before the record named this commit: a caller waiting is told
                    w.on_disk = w.on_disk.max(p.pins[1]);
                }
                p.sync = if rules.data_before_record { At::SyncUnpin } else { At::SyncData };
                Ok(())
            }),
            At::SyncUnpin => step("moves its pin", &|w, p| {
                if !rules.data_before_record {
                    w.on_disk = w.on_disk.max(p.pins[1]);
                }
                let writing = p.write != At::Idle && (rules.pin_kept_for_a_write || p.write == At::WriteAwaitsSync);
                p.pins = if writing || owes(p) { [p.pins[1], NONE] } else { [NONE; 2] };
                p.sync = At::Idle;
                p.captured = NONE;
                Ok(())
            }),
            _ => Ok(()),
        };
        sync.map_err(|what| self.broken(what))
    }

    /// A write's transaction begun: the page it replaces, each in turn, and the page it writes, which LMDB gives it
    /// from those freed before the earliest reader's commit and before the newest, or a fresh one
    fn begin(&self, world: &World, at: usize, next: &mut Vec<(String, World)>) -> Result<(), Broken> {
        let commit = world.newest + 1;
        let snapshot = world.snapshots[usize::from(world.newest)];
        let before = world.newest.min(world.oldest_reader());
        let freed = (0..PAGES).filter(|page| snapshot.free[*page] < before).min_by_key(|page| snapshot.free[*page]);
        let fresh = (0..PAGES).find(|page| snapshot.free[*page] == FRESH);
        let Some(written) = freed.or(fresh).filter(|_| usize::from(commit) < COMMITS) else {
            // No page to write, or no commit left within the model's bound: the write is given up
            let mut after = world.clone();
            after.writer = NONE;
            after.processes[at].write = At::Idle;
            next.push((format!("{at} gives its write up"), after));
            return Ok(());
        };
        for replaced in (0..PAGES).filter(|page| snapshot.tree[*page] != NONE) {
            let mut after = world.clone();
            after.processes[at].writing = [commit, replaced as u8, written as u8];
            after.processes[at].write = At::WritePages;
            next.push((format!("{at} begins commit {commit}, page {replaced} to page {written}"), after.clone()));
            if self.bounds.spills {
                write_page(&mut after, written as u8, commit).map_err(|what| self.broken(what))?;
                next.push((format!("{at} begins commit {commit} and writes page {written} before it"), after));
            }
        }
        Ok(())
    }

    /// Power is lost: every process is gone with LMDB's lock file, and of what was written and not synced each
    /// page, each meta page and the record is as the disk had it or as the OS did, in every combination. A meta
    /// page a commit was part way through writing is as the disk had it
    fn lose(&self, world: &World, next: &mut Vec<(String, World)>) {
        let mut unsynced: Vec<usize> =
            (0..PAGES).filter(|page| world.os_pages[*page] != world.disk_pages[*page]).collect();
        for slot in 0..2 {
            let written = world.os_metas[slot];
            if written != world.disk_metas[slot] && written.body == written.id {
                unsynced.push(PAGES + slot);
            }
        }
        if world.os_record != world.disk_record {
            unsynced.push(PAGES + 2);
        }
        for kept in 0..1u32 << unsynced.len() {
            let mut after = world.clone();
            after.os_pages = world.disk_pages;
            after.os_metas = world.disk_metas;
            after.os_record = world.disk_record;
            for (bit, what) in unsynced.iter().enumerate() {
                if kept & (1 << bit) == 0 {
                    continue;
                }
                match *what {
                    page if page < PAGES => after.os_pages[page] = world.os_pages[page],
                    slot if slot < PAGES + 2 => after.os_metas[slot - PAGES] = world.os_metas[slot - PAGES],
                    _ => after.os_record = world.os_record,
                }
            }
            after.disk_pages = after.os_pages;
            after.disk_metas = after.os_metas;
            after.disk_record = after.os_record;
            after.processes = [NO_PROCESS; PROCESSES];
            after.newest = NONE;
            after.writer = NONE;
            after.boot += 1;
            after.lost = true;
            after.losses += 1;
            next.push((format!("power is lost, keeping {kept:b} of what {} had written", unsynced.len()), after));
        }
    }

    /// A process opens the database as the design says, and what it opens is held to what was promised: after a
    /// loss, a commit whose every page is as it was written and no earlier than any a caller was told is on disk;
    /// after deaths alone, the newest commit that returned
    fn open(&self, world: &World, at: usize) -> Result<World, Broken> {
        let mut after = world.clone();
        after.opens += 1;
        let others = world.processes.iter().any(|process| process.alive);
        let record = world.os_record;
        let newer = World::newer_meta(&world.os_metas);
        let restore = record.commit != NONE && !others && record.boot != world.boot && record.commit < newer.id;
        if restore {
            after.os_metas = [Meta { body: record.body, id: record.commit }; 2];
            after.disk_metas = after.os_metas;
            after.disk_pages = after.os_pages;
        }
        if record.commit != NONE && !others && record.boot != world.boot && record.commit <= newer.id {
            after.os_record.boot = world.boot;
            after.disk_record = after.os_record;
        }
        if !others {
            // The one process to have the database open makes LMDB's lock file again, from the file's newer meta page
            after.newest = World::newer_meta(&after.os_metas).id;
            for process in after.processes.iter_mut() {
                process.pins = [NONE; 2];
            }
        }
        // LMDB's newest commit is its lock file's, which the one process to have the database open has just made
        // from the file, and its trees are those the meta page of that number names
        let opened = after.os_metas[usize::from(after.newest & 1)];
        if opened.id != after.newest && !others {
            return Err(self.broken("an open whose newest commit is not its meta page's"));
        }
        let opened = Meta { body: opened.body, id: after.newest };
        if opened.body != opened.id {
            return Err(self.broken("an open at a meta page that names one commit's trees under another's number"));
        }
        let tree = after.snapshots[usize::from(opened.id)].tree;
        if (0..PAGES).any(|page| tree[page] != NONE && after.os_pages[page] != tree[page]) {
            return Err(self.broken("an open at a commit one of whose pages is not as it was written"));
        }
        if world.lost && opened.id < world.on_disk {
            return Err(self.broken("an open after a loss at an earlier commit than one a caller was told is on disk"));
        }
        if !world.lost && opened.id < world.returned {
            return Err(self.broken("an open after a process's death at an earlier commit than one that returned"));
        }
        if world.lost {
            // What returned after the commit a loss opens at is gone, as the grouped mode says it may be
            after.returned = opened.id;
        }
        after.lost = false;
        after.processes[at] = Process { alive: true, ..NO_PROCESS };
        Ok(after)
    }

    /// Every world the first leads to, depth first, or the first thing broken
    fn run(&mut self, world: World) -> Result<(), Broken> {
        if !self.seen.insert(world.clone()) {
            return Ok(());
        }
        for (name, after) in self.steps(&world)? {
            self.path.push(name);
            self.run(after)?;
            self.path.pop();
        }
        Ok(())
    }
}

/// A page written to the file as the OS holds it. No page of the commit the record on disk names may be: it is the
/// commit a loss opens at
fn write_page(world: &mut World, page: u8, version: u8) -> Result<(), &'static str> {
    let kept = world.disk_record.commit;
    if kept != NONE && world.snapshots[usize::from(kept)].tree[usize::from(page)] != NONE {
        return Err("a write over a page of the commit the record on disk names");
    }
    world.os_pages[usize::from(page)] = version;
    Ok(())
}

/// Runs the model under `rules` from a new database two processes may open, and says what it found broken, if
/// anything, with how many worlds it went through
fn explore(rules: Rules, bounds: Bounds) -> (Result<(), Broken>, usize) {
    let mut search = Search { rules, bounds, seen: HashSet::new(), path: Vec::new() };
    let found = search.run(World::new());
    (found, search.seen.len())
}

const BOUNDS: Bounds = Bounds { deaths: 1, losses: 1, opens: 4, spills: true };

#[test]
fn the_design_s_rules_hold_through_every_order_of_steps_within_the_bounds() {
    let (found, worlds) = explore(DESIGN, BOUNDS);
    if let Err((what, path)) = &found {
        let from = path.len().saturating_sub(40);
        panic!("{what}, after {} steps, the last:\n  {}", path.len(), path[from..].join("\n  "));
    }
    // A run that went nowhere would hold anything
    assert!(worlds > 10_000, "{worlds} worlds");
}

/// The model without one rule: what it then finds broken
fn without(rules: Rules) -> &'static str {
    match explore(rules, BOUNDS).0 {
        Ok(()) => "nothing",
        Err((what, _)) => what,
    }
}

/// What a commit the record names being written over shows as: the write itself, or the open after the loss that
/// finds it, whichever order of steps the search meets first
const WRITTEN_OVER: [&str; 2] = [
    "a write over a page of the commit the record on disk names",
    "an open at a commit one of whose pages is not as it was written",
];

#[test]
fn each_rule_s_absence_is_found() {
    // A write that takes its pin at its commit has by then written pages, where it is too large for its memory
    assert!(WRITTEN_OVER.contains(&without(Rules { pin_before_write: false, ..DESIGN })));
    // With the old pin ended before the record names the new one, the commit the record on disk still names is
    // nobody's, and a write takes its pages
    assert!(WRITTEN_OVER.contains(&without(Rules { old_pin_until_record: false, ..DESIGN })));
    // A record another process wrote and died before syncing is no record on disk
    assert!(WRITTEN_OVER.contains(&without(Rules { record_synced_by_reader: false, ..DESIGN })));
    // A record synced before the data it names is a commit that isn't on disk
    assert!(WRITTEN_OVER.contains(&without(Rules { data_before_record: false, ..DESIGN })));
    // A record written back by the slower of two syncs undoes a commit a caller was told is on disk
    assert_eq!(
        without(Rules { record_never_back: false, ..DESIGN }),
        "an open after a loss at an earlier commit than one a caller was told is on disk"
    );
    // A new database's two meta pages are of one commit, and its first sync never takes one
    assert_eq!(
        without(Rules { slot: Slot::OtherEarlier, ..DESIGN }),
        "a sync that reads its meta page again and again, and never takes it"
    );
    // A writer that died with its commit's meta page written and LMDB's lock file not told leaves the other page
    // later for good
    assert_eq!(
        without(Rules { slot: Slot::OtherNotLater, ..DESIGN }),
        "a sync that reads its meta page again and again, and never takes it"
    );
    // A meta page read beside the commit that writes over it names that commit's trees under the pin's number
    assert_eq!(
        without(Rules { slot: Slot::Unchecked, ..DESIGN }),
        "an open at a meta page that names one commit's trees under another's number"
    );
}

/// What the model finds without each rule, and how many worlds the design's run goes through: printed, for whoever
/// changes the model or its bounds
#[test]
#[ignore = "a reading, run by hand: cargo nextest run -p elmdb --run-ignored only -E 'test(the_model_s_readings)' --no-capture"]
fn the_model_s_readings() {
    let began = std::time::Instant::now();
    let (found, worlds) = explore(DESIGN, BOUNDS);
    println!(
        "design: {:?}, {worlds} worlds, {:.1?}",
        found.map_err(|(what, path)| (what, path.len())),
        began.elapsed()
    );
    let mutations: [(&str, Rules); 9] = [
        ("pin at the commit", Rules { pin_before_write: false, ..DESIGN }),
        ("record may go back", Rules { record_never_back: false, ..DESIGN }),
        ("old pin ended early", Rules { old_pin_until_record: false, ..DESIGN }),
        ("record before data", Rules { data_before_record: false, ..DESIGN }),
        ("unsynced record relied on", Rules { record_synced_by_reader: false, ..DESIGN }),
        ("no pin kept for a write", Rules { pin_kept_for_a_write: false, ..DESIGN }),
        ("other page earlier", Rules { slot: Slot::OtherEarlier, ..DESIGN }),
        ("other page not later", Rules { slot: Slot::OtherNotLater, ..DESIGN }),
        ("page unchecked", Rules { slot: Slot::Unchecked, ..DESIGN }),
    ];
    for (name, rules) in mutations {
        let began = std::time::Instant::now();
        let (found, worlds) = explore(rules, BOUNDS);
        let found = found.map_err(|(what, path)| (what, path.len()));
        println!("{name}: {found:?}, {worlds} worlds, {:.1?}", began.elapsed());
    }
}
