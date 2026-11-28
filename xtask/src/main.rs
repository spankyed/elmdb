//! The repository's own tasks, run as `cargo xtask <task>`:
//!
//! - `generate`: writes the TypeScript declarations of the protocol's types into `crates/elmdb-node/protocol/`, and
//!   the schema's JSON Schema, whose `$id` is the schema language's URL.
//! - `check-generated`: fails when what `generate` would write differs from what is committed.
//! - `notices`: writes the npm package's notices into `crates/elmdb-node/`: `THIRD-PARTY-NOTICES`, the licenses and
//!   notices of the crates its `.node` files are built from, with cargo-about (`CARGO_ABOUT`) and the configuration and
//!   template beside it; the Rust standard library's, from the toolchain; and copies of the repository's license files.
//! - `check-notices`: fails when what `notices` would write differs from what is committed, or when package.json's
//!   `files` leaves one of those files out of the package.
//! - `check-versions`: fails unless the workspace's `Cargo.toml`, the binding's `package.json` and EARS' hold one
//!   version, EARS' peer `elmdb` names exactly it, so the pair a release packs installs only together, the binding's
//!   `optionalDependencies` name each of its `napi.targets`' platform package at exactly it, and `napi.targets` matches
//!   the targets declared in `deny.toml` and `.github/workflows/release.yml`.
//! - `check-workspace-deps`: fails when root `Cargo.toml`'s `[workspace.dependencies]` declares a dependency that no
//!   workspace member crate references via `workspace = true`.
//! - `check-one-lmdb`: fails unless the dependency tree holds exactly one LMDB `-sys` crate (one version of one).
//! - `check-exact`: fails unless every ICU crate the lockfile holds, and each crate whose output an index stores
//!   (`INDEXED`), is a dependency of `elmdb` required at exactly the locked version, so whoever builds elmdb builds the
//!   collator its stored sort keys are stamped with, and the tokenizer, stemmer and graph its indexes were built by.
//! - `check-diagnostics-off`: fails when a default build of the workspace has `elmdb` depend on `tracing` itself,
//!   which only its `diagnostics` feature may make it do.
//! - `check-leiden-serial`: fails when the workspace builds `leiden-rs` with any feature, `rayon` among them, whose
//!   parallel moves would make a partition depend on the threads' timing rather than on its seed alone.
//! - `check-sync`: fails when v2 engine crate code outside `elmdb-storage/src/locks.rs` locks, waits or keeps per-thread
//!   state through `std` or `parking_lot` directly rather than through `elmdb-storage::locks`, unless the file is listed
//!   in `UNMODELLED` with its reason; a listed file that no longer does is stale.
//! - `check-commits`: fails when engine code outside `elmdb-storage` / `elmdb-lmdb` commits a write transaction directly,
//!   so every commit passes through the `DurabilityGate`.
//! - `mutant-exclusions`: writes `target/mutants.toml`, the configuration a mutation run reads with `--config`, and
//!   prints its path: `.cargo/mutants.toml` with an `exclude_re` for each `// mutant-skip:` comment in the source, which
//!   sits above the line it excuses and holds the reason. Nothing it writes is committed, so the comment is the one
//!   place a line's exclusion lives, and no merge has one to reconcile. It writes nothing from a `.cargo/mutants.toml`
//!   whose `exclude_re` names a line, so no run reads one.
//! - `check-mutant-exclusions`: fails when a `// mutant-skip:` comment, or an `exclude_re` of `.cargo/mutants.toml`,
//!   matches none of the mutants cargo-mutants lists, so the mutant it was written for is reported again; and when an
//!   `exclude_re` there names a line, which only a comment may.
//! - `compare`: runs one command on two revisions in turn and prints each reading beside the load as its run ended; it
//!   reports and decides nothing (`docs/architecture/testing.md`, Comparing two revisions).
//! - `sweep`: bounds each build directory this checkout uses, its target, the models' and the scratch target, removing
//!   what no recent build used, through `cargo-sweep`, and the incremental directories it leaves (`docs/architecture/
//!   testing.md`, The build directory).
//! - `scratch-target`: makes and prints the directory a mutated or scratch copy of the tree builds into, beside this
//!   checkout's target, which `sweep` bounds with the rest.
//! - `pre-push`: every check CI runs on a change, the change's mutants included, cheapest first, stopping at the first
//!   that fails: what to run before a push. Among them is `deny.toml`'s, which CI runs as `cargo deny` itself: the
//!   bans, the sources and the RustSec advisories.

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

mod build_dir;
mod compare;
mod generate;
mod pre_push;

fn main() -> ExitCode {
    let task = std::env::args().nth(1);
    let result = match task.as_deref() {
        Some("generate") => generate::write(&generated_dir()),
        Some("compare") => compare::compare(&workspace_root(), &std::env::args().skip(2).collect::<Vec<_>>()),
        Some("check-generated") => check_generated(),
        Some("notices") => write_notices(&workspace_root()),
        Some("check-notices") => check_notices(&workspace_root()),
        Some("check-versions") => check_versions(&workspace_root()),
        Some("check-workspace-deps") => check_workspace_deps(&workspace_root()),
        Some("check-one-lmdb") => check_one_lmdb(&workspace_root()),
        Some("check-exact") => check_exact(&workspace_root()),
        Some("check-diagnostics-off") => check_diagnostics_off(&workspace_root()),
        Some("check-failpoints-off") => check_failpoints_off(&workspace_root()),
        Some("check-leiden-serial") => check_leiden_serial(&workspace_root()),
        Some("sweep") => build_dir::sweep(&workspace_root()),
        Some("scratch-target") => scratch_target(&workspace_root()),
        Some("check-sync") => check_sync(&workspace_root()),
        Some("check-commits") => check_commits(&workspace_root()),
        Some("mutant-exclusions") => mutant_exclusions(&workspace_root()),
        Some("check-mutant-exclusions") => check_mutant_exclusions(&workspace_root()),
        Some("pre-push") => {
            let root = workspace_root();
            pre_push::pre_push(
                &root,
                &std::env::args().skip(2).collect::<Vec<_>>(),
                &[
                    ("generated protocol", &check_generated),
                    ("one version", &|| check_versions(&root)),
                    ("workspace dependencies used", &|| check_workspace_deps(&root)),
                    ("one LMDB", &|| check_one_lmdb(&root)),
                    ("exact crates", &|| check_exact(&root)),
                    ("diagnostics off by default", &|| check_diagnostics_off(&root)),
                    ("failpoints off by default", &|| check_failpoints_off(&root)),
                    ("serial Leiden", &|| check_leiden_serial(&root)),
                    ("elmdb-storage::locks", &|| check_sync(&root)),
                    ("one place commits", &|| check_commits(&root)),
                    ("third-party notices", &|| check_notices(&root)),
                    ("bans, sources and advisories", &|| check_deny(env!("CARGO"), &root)),
                    ("mutant exclusions", &|| check_mutant_exclusions(&root)),
                ],
            )
        }
        _ => Err(
            "usage: cargo xtask <generate | check-generated | notices | check-notices | check-versions | check-workspace-deps | check-one-lmdb | check-exact | check-diagnostics-off | check-failpoints-off | check-leiden-serial | check-sync | check-commits | mutant-exclusions | check-mutant-exclusions | compare | sweep | scratch-target | pre-push>"
                .to_owned(),
        ),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("{message}");
            ExitCode::FAILURE
        }
    }
}

/// v2 engine crates scanned by `check-sync` to keep direct locking outside `elmdb-storage::locks` explicit and audited.
const SYNC_CHECKED_CRATES: &[&str] = &[
    "elmdb",
    "elmdb-engine",
    "elmdb-invalidation",
    "elmdb-lmdb",
    "elmdb-projections",
    "elmdb-query",
    "elmdb-sim",
    "elmdb-storage",
    "elmdb-vec",
    "elmdb-wire",
];

/// The engine's files (prefixed `<crate>/src/<rel>`) that lock, wait or keep per-thread state outside
/// `elmdb-storage::locks`, each with why. A listed file that no longer locks is flagged as stale.
const UNMODELLED: &[(&str, &str)] = &[
    (
        "elmdb/src/diagnostics.rs",
        "optional tracing span collector installed by host program, outside engine concurrency",
    ),
    (
        "elmdb-lmdb/src/env.rs",
        "process-wide OPEN_ENVS Mutex preventing same-process duplicate LMDB opens (POSIX fcntl lock hazard)",
    ),
];

/// Fails on a v2 engine file that locks outside `elmdb-storage::locks` and isn't in `UNMODELLED`, or on a listed file
/// that no longer locks outside `elmdb-storage::locks`.
fn check_sync(root: &Path) -> Result<(), String> {
    let mut found = std::collections::BTreeSet::new();
    for crate_name in SYNC_CHECKED_CRATES {
        let source = root.join("crates").join(crate_name).join("src");
        if !source.is_dir() {
            continue;
        }
        for rel in unmodelled_files(&source)? {
            let full = format!("{crate_name}/src/{rel}");
            if full == "elmdb-storage/src/locks.rs" {
                continue;
            }
            found.insert(full);
        }
    }
    let listed: std::collections::BTreeSet<String> = UNMODELLED.iter().map(|(file, _)| (*file).to_owned()).collect();
    let unlisted: Vec<_> = found.difference(&listed).collect();
    let stale: Vec<_> = listed.difference(&found).collect();
    if unlisted.is_empty() && stale.is_empty() {
        return Ok(());
    }
    let mut message = String::new();
    if !unlisted.is_empty() {
        message += &format!(
            "these lock, wait or keep per-thread state outside elmdb-storage::locks: \
             {unlisted:?}; use elmdb-storage::locks primitives, or list the file in xtask's UNMODELLED with why\n"
        );
    }
    if !stale.is_empty() {
        message += &format!(
            "UNMODELLED lists files that no longer lock outside elmdb-storage::locks: {stale:?}; remove them\n"
        );
    }
    Err(message.trim_end().to_owned())
}

/// Fails when root `Cargo.toml`'s `[workspace.dependencies]` declares a dependency that no workspace member
/// references via `workspace = true`.
fn check_workspace_deps(root: &Path) -> Result<(), String> {
    let root_manifest_path = root.join("Cargo.toml");
    let root_text =
        std::fs::read_to_string(&root_manifest_path).map_err(|e| format!("{}: {e}", root_manifest_path.display()))?;
    let declared = parse_workspace_dependency_keys(&root_text);
    let members = parse_workspace_members(&root_text);
    if members.is_empty() {
        return Err("root Cargo.toml lists no [workspace] members".to_owned());
    }
    let mut used = std::collections::BTreeSet::new();
    for member in &members {
        let member_manifest = root.join(member).join("Cargo.toml");
        let text =
            std::fs::read_to_string(&member_manifest).map_err(|e| format!("{}: {e}", member_manifest.display()))?;
        collect_used_workspace_deps(&text, &mut used);
    }
    let unused: Vec<&String> = declared.difference(&used).collect();
    if unused.is_empty() {
        return Ok(());
    }
    Err(format!(
        "unused [workspace.dependencies] in Cargo.toml: {unused:?}; remove dead entries or reference them from a member crate"
    ))
}

fn strip_toml_comment(line: &str) -> &str {
    let mut in_single = false;
    let mut in_double = false;
    let mut escaped = false;
    for (idx, ch) in line.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match ch {
            '\\' if in_double => escaped = true,
            '"' if !in_single => in_double = !in_double,
            '\'' if !in_double => in_single = !in_single,
            '#' if !in_single && !in_double => return &line[..idx],
            _ => {}
        }
    }
    line
}

/// Joins multiline TOML arrays (`[...]`) and inline tables (`{...}`) into single logical statements,
/// stripping `#` comments while respecting `'...'` and `"..."` string literals.
fn toml_logical_lines(manifest: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut buf = String::new();
    let mut depth = 0usize;
    for raw_line in manifest.lines() {
        let line = strip_toml_comment(raw_line).trim();
        if line.is_empty() {
            continue;
        }
        if depth == 0 && line.starts_with('[') && line.ends_with(']') {
            if !buf.is_empty() {
                out.push(std::mem::take(&mut buf));
            }
            out.push(line.to_owned());
            continue;
        }
        if !buf.is_empty() {
            buf.push(' ');
        }
        buf.push_str(line);

        let mut in_single = false;
        let mut in_double = false;
        let mut escaped = false;
        for ch in line.chars() {
            if escaped {
                escaped = false;
                continue;
            }
            match ch {
                '\\' if in_double => escaped = true,
                '"' if !in_single => in_double = !in_double,
                '\'' if !in_double => in_single = !in_single,
                '{' | '[' if !in_single && !in_double => depth += 1,
                '}' | ']' if !in_single && !in_double => depth = depth.saturating_sub(1),
                _ => {}
            }
        }
        if depth == 0 && !buf.is_empty() {
            out.push(std::mem::take(&mut buf));
        }
    }
    if !buf.is_empty() {
        out.push(buf);
    }
    out
}

/// Splits a dotted TOML key or table header body (without outer `[` / `]`) into unquoted segments,
/// respecting `'...'` and `"..."` quoted segments that contain dots (e.g. `target.'cfg(unix)'.dependencies`).
fn split_toml_key_segments(raw: &str) -> Vec<String> {
    let mut segs = Vec::new();
    let mut cur = String::new();
    let mut in_single = false;
    let mut in_double = false;
    let mut escaped = false;
    for ch in raw.chars() {
        if escaped {
            cur.push(ch);
            escaped = false;
            continue;
        }
        match ch {
            '\\' if in_double => escaped = true,
            '"' if !in_single => in_double = !in_double,
            '\'' if !in_double => in_single = !in_single,
            '.' if !in_single && !in_double => {
                let s = cur.trim().to_owned();
                if !s.is_empty() {
                    segs.push(s);
                }
                cur.clear();
            }
            _ => cur.push(ch),
        }
    }
    let s = cur.trim().to_owned();
    if !s.is_empty() {
        segs.push(s);
    }
    segs
}

/// Extracts all single- or double-quoted TOML string literals from `text`.
fn extract_toml_strings(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '"' || ch == '\'' {
            let quote = ch;
            let mut s = String::new();
            let mut escaped = false;
            for c in chars.by_ref() {
                if escaped {
                    s.push(c);
                    escaped = false;
                    continue;
                }
                if quote == '"' && c == '\\' {
                    escaped = true;
                    continue;
                }
                if c == quote {
                    break;
                }
                s.push(c);
            }
            out.push(s);
        }
    }
    out
}

fn parse_workspace_dependency_keys(manifest: &str) -> std::collections::BTreeSet<String> {
    let mut keys = std::collections::BTreeSet::new();
    let mut in_ws_deps = false;
    for line in toml_logical_lines(manifest) {
        if line.starts_with('[') && line.ends_with(']') {
            let segs = split_toml_key_segments(&line[1..line.len() - 1]);
            if segs.len() == 2 && segs[0] == "workspace" && segs[1] == "dependencies" {
                in_ws_deps = true;
            } else if segs.len() >= 3 && segs[0] == "workspace" && segs[1] == "dependencies" {
                keys.insert(segs[2].clone());
                in_ws_deps = false;
            } else {
                in_ws_deps = false;
            }
            continue;
        }
        if !in_ws_deps {
            continue;
        }
        if let Some((lhs, _)) = line.split_once('=')
            && let Some(first) = split_toml_key_segments(lhs).into_iter().next()
        {
            keys.insert(first);
        }
    }
    keys
}

fn parse_workspace_members(manifest: &str) -> Vec<String> {
    let mut in_ws = false;
    for line in toml_logical_lines(manifest) {
        if line.starts_with('[') && line.ends_with(']') {
            let segs = split_toml_key_segments(&line[1..line.len() - 1]);
            in_ws = segs.len() == 1 && segs[0] == "workspace";
            continue;
        }
        if !in_ws {
            continue;
        }
        if let Some((lhs, rhs)) = line.split_once('=')
            && lhs.trim() == "members"
        {
            return extract_toml_strings(rhs);
        }
    }
    Vec::new()
}

enum DepSection {
    Other,
    Table,
    Subtable(String),
}

fn classify_dep_section(header: &str) -> DepSection {
    let segs = split_toml_key_segments(&header[1..header.len() - 1]);
    let is_dep_kind = |s: &str| matches!(s, "dependencies" | "dev-dependencies" | "build-dependencies");
    match segs.as_slice() {
        [kind] if is_dep_kind(kind) => DepSection::Table,
        [kind, dep] if is_dep_kind(kind) => DepSection::Subtable(dep.clone()),
        [target, _, kind] if target == "target" && is_dep_kind(kind) => DepSection::Table,
        [target, _, kind, dep] if target == "target" && is_dep_kind(kind) => DepSection::Subtable(dep.clone()),
        _ => DepSection::Other,
    }
}

fn collect_used_workspace_deps(manifest: &str, used: &mut std::collections::BTreeSet<String>) {
    let ws_true_re = regex::Regex::new(r"\bworkspace\s*=\s*true\b").expect("valid regex");
    let pkg_re = regex::Regex::new(r#"\bpackage\s*=\s*(?:"([^"]+)"|'([^']+)')"#).expect("valid regex");
    let mut section = DepSection::Other;
    let mut sub_ws_true = false;
    let mut sub_pkg: Option<String> = None;

    let flush_subtable = |sec: &DepSection,
                          ws_true: &mut bool,
                          pkg: &mut Option<String>,
                          used: &mut std::collections::BTreeSet<String>| {
        if let DepSection::Subtable(dep_name) = sec
            && *ws_true
        {
            used.insert(dep_name.clone());
            if let Some(p) = pkg.take() {
                used.insert(p);
            }
        }
        *ws_true = false;
        *pkg = None;
    };

    for line in toml_logical_lines(manifest) {
        if line.starts_with('[') && line.ends_with(']') {
            flush_subtable(&section, &mut sub_ws_true, &mut sub_pkg, used);
            section = classify_dep_section(&line);
            continue;
        }
        let Some((lhs, rhs)) = line.split_once('=') else {
            continue;
        };
        let rhs = rhs.trim();
        match &section {
            DepSection::Other => {}
            DepSection::Table => {
                let segs = split_toml_key_segments(lhs);
                if segs.len() == 2 && segs[1] == "workspace" {
                    if rhs == "true" {
                        used.insert(segs[0].clone());
                    }
                } else if segs.len() == 1 && ws_true_re.is_match(rhs) {
                    used.insert(segs[0].clone());
                    if let Some(cap) = pkg_re.captures(rhs)
                        && let Some(m) = cap.get(1).or_else(|| cap.get(2))
                    {
                        used.insert(m.as_str().to_owned());
                    }
                }
            }
            DepSection::Subtable(_) => {
                let segs = split_toml_key_segments(lhs);
                if segs.len() == 1 && segs[0] == "workspace" && rhs == "true" {
                    sub_ws_true = true;
                } else if segs.len() == 1 && segs[0] == "package" {
                    sub_pkg = extract_toml_strings(rhs).into_iter().next();
                }
            }
        }
    }
    flush_subtable(&section, &mut sub_ws_true, &mut sub_pkg, used);
}

/// A `// mutant-skip: PATTERN` comment, ` (as above)` for a reason given by the one before it. Its reason follows it, as a comment
const MUTANT_SKIP: &str = "// mutant-skip: ";

/// The configuration cargo-mutants reads by default, which is committed and names no line
const CONFIG: &str = ".cargo/mutants.toml";
/// The one a run reads, relative to the workspace root: `CONFIG` with the comments' exclusions, never committed
const DERIVED_CONFIG: &str = "target/mutants.toml";

/// One `// mutant-skip:` comment: the file and line it is on, and the `exclude_re` it makes, which names the first
/// line after it that isn't a comment
#[derive(Debug, PartialEq)]
struct Skip {
    path: String,
    line: usize,
    pattern: String,
}

/// A file's `// mutant-skip:` comments, in line order
fn skips_in(path: &str, source: &str) -> Result<Vec<Skip>, String> {
    let lines: Vec<&str> = source.lines().collect();
    let mut skips = Vec::new();
    for (at, line) in lines.iter().enumerate() {
        let Some(pattern) = line.trim_start().strip_prefix(MUTANT_SKIP) else { continue };
        let pattern = pattern.strip_suffix(" (as above)").unwrap_or(pattern);
        let code = lines[at..]
            .iter()
            .position(|l| !l.trim_start().starts_with("//") && !l.trim().is_empty())
            .ok_or_else(|| format!("{path}:{}: a mutant-skip with no code after it", at + 1))?;
        skips.push(Skip {
            path: path.to_owned(),
            line: at + 1,
            pattern: format!("{path}:{}:.*{pattern}", at + code + 1),
        });
    }
    Ok(skips)
}

/// Every crate's `// mutant-skip:` comments, in file and line order
fn skips(root: &Path) -> Result<Vec<Skip>, String> {
    fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), String> {
        for entry in std::fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))? {
            let path = entry.map_err(|e| e.to_string())?.path();
            if path.is_dir() {
                rust_files(&path, out)?;
            } else if path.extension().is_some_and(|e| e == "rs") {
                out.push(path);
            }
        }
        Ok(())
    }
    let crates = root.join("crates");
    let mut files = Vec::new();
    for entry in std::fs::read_dir(&crates).map_err(|e| format!("{}: {e}", crates.display()))? {
        let source = entry.map_err(|e| e.to_string())?.path().join("src");
        if source.is_dir() {
            rust_files(&source, &mut files)?;
        }
    }
    files.sort();
    let mut found = Vec::new();
    for file in files {
        let text = std::fs::read_to_string(&file).map_err(|e| format!("{}: {e}", file.display()))?;
        let path = file.strip_prefix(root).map_err(|e| e.to_string())?.to_string_lossy().replace('\\', "/");
        found.extend(skips_in(&path, &text)?);
    }
    Ok(found)
}

/// Where `exclude_re`'s list starts in the configuration's text, and where the line closing it does
fn exclude_re_list(text: &str) -> Result<(usize, usize), String> {
    let start = 1 + text.find("\nexclude_re = [").ok_or(format!("no exclude_re list in {CONFIG}"))?;
    let end = text[start..].find("\n]").ok_or("exclude_re's list doesn't close")?;
    Ok((start, start + end + 1))
}

/// Whether a committed `exclude_re` names a line, as `file.rs:12:` or `file.rs:(12|34):`: a line's number in a
/// committed file is rewritten by every edit above the line, and two branches that each make one conflict in it
fn names_a_line(pattern: &str) -> bool {
    pattern.match_indices(".rs:").any(|(at, found)| {
        pattern[at + found.len()..].trim_start_matches('(').starts_with(|c: char| c.is_ascii_digit())
    })
}

/// What a committed `exclude_re` that names a line is refused with
fn line_named(pattern: &str) -> String {
    format!(
        "{CONFIG}: the exclude_re {pattern:?} names a line, which every edit above that line would move; remove it, \
         and excuse the mutant with a `// mutant-skip:` comment above its line"
    )
}

/// The configuration a run reads: the committed one's text with each comment's exclusion at the end of `exclude_re`.
/// A committed pattern that names a line is refused here too, so a run never reads one: a list of them kept through
/// a merge would excuse whatever sits on its lines now
fn with_skips(text: &str, skips: &[Skip]) -> Result<String, String> {
    let (start, close) = exclude_re_list(text)?;
    if let Some(pattern) = exclude_res(text)?.iter().find(|pattern| names_a_line(pattern)) {
        return Err(line_named(pattern));
    }
    let last = text[start..close].lines().map(str::trim).rfind(|line| !line.is_empty() && !line.starts_with('#'));
    if last.is_some_and(|line| !line.ends_with(',')) {
        return Err(format!(
            "exclude_re's last pattern in {CONFIG} has no comma after it, which the patterns written after it need"
        ));
    }
    let mut derived = format!(
        "# Written by `cargo xtask mutant-exclusions` and never committed: {CONFIG}, and at the end of its\n\
         # exclude_re a pattern for each `// mutant-skip:` comment in the source. Edit those, not this.\n\n{}",
        &text[..close]
    );
    for skip in skips {
        derived += &format!("    \"{}\",\n", skip.pattern.replace('\\', "\\\\").replace('"', "\\\""));
    }
    Ok(derived + &text[close..])
}

fn mutant_exclusions(root: &Path) -> Result<(), String> {
    let config = root.join(CONFIG);
    let text = std::fs::read_to_string(&config).map_err(|e| format!("{}: {e}", config.display()))?;
    let derived = root.join(DERIVED_CONFIG);
    let written = |e: std::io::Error| format!("{}: {e}", derived.display());
    std::fs::create_dir_all(derived.parent().expect("a file in target/")).map_err(written)?;
    std::fs::write(&derived, with_skips(&text, &skips(root)?)?).map_err(written)?;
    // The path alone, for `--config "$(cargo xtask mutant-exclusions)"` and for `scripts/mutants-in-diff.py`
    println!("{}", derived.display());
    Ok(())
}

fn check_mutant_exclusions(root: &Path) -> Result<(), String> {
    let config = root.join(CONFIG);
    let text = std::fs::read_to_string(&config).map_err(|e| format!("{}: {e}", config.display()))?;
    // Every mutant, the configuration's exclusions and globs not applied, so each exclusion is tried against them all;
    // uncoloured, since CI's `CARGO_TERM_COLOR=always` would wrap each name's parts in escapes no pattern matches
    let output = Command::new("cargo")
        .args(["mutants", "--list", "--no-config", "--colors", "never"])
        .current_dir(root)
        .output()
        .map_err(|e| {
            format!("cargo mutants --list: {e}; install cargo-mutants, as .github/workflows/mutants.yml pins it")
        })?;
    if !output.status.success() {
        return Err(format!("cargo mutants --list failed: {}", String::from_utf8_lossy(&output.stderr).trim()));
    }
    let listed = String::from_utf8(output.stdout).map_err(|e| e.to_string())?;
    let mutants: Vec<&str> = listed.lines().filter(|line| !line.is_empty()).collect();
    if mutants.is_empty() {
        return Err("cargo mutants --list listed no mutants, so no exclusion could be tried".to_owned());
    }
    let skips = skips(root)?;
    let stale = stale_exclusions(&exclude_res(&text)?, &skips, &mutants)?;
    if !stale.is_empty() {
        return Err(stale.join("\n"));
    }
    // What a run reads has to be writable from this text, which no run in this job would otherwise find out
    with_skips(&text, &skips).map(|_| ())
}

/// What is wrong with the exclusions, given every mutant cargo-mutants lists: a committed `exclude_re` that names a
/// line, or one that matches no mutant, and a `// mutant-skip:` comment whose pattern matches no mutant of the line
/// under it
fn stale_exclusions(committed: &[String], skips: &[Skip], mutants: &[&str]) -> Result<Vec<String>, String> {
    let matches_none = |pattern: &str| -> Result<bool, String> {
        let pattern = regex::Regex::new(pattern).map_err(|e| format!("exclusion {pattern:?}: {e}"))?;
        Ok(!mutants.iter().any(|mutant| pattern.is_match(mutant)))
    };
    let mut stale = Vec::new();
    for pattern in committed {
        if names_a_line(pattern) {
            stale.push(line_named(pattern));
        } else if matches_none(pattern)? {
            stale.push(format!(
                "{CONFIG}: the exclude_re {pattern:?} matches none of the {} mutants listed; name the mutant as \
                 `cargo mutants --list --no-config` does now, or remove it",
                mutants.len()
            ));
        }
    }
    for skip in skips {
        // A comment excuses mutants of the line under it. One whose pattern matches a mutant of any other line
        // holds an operator where it meant a character, a `|` above all, which with nothing on one side of it
        // matches every mutant there is: no mutant is then tested, and nothing fails
        let own = skip.pattern.splitn(3, ':').take(2).collect::<Vec<_>>().join(":") + ":";
        let pattern = regex::Regex::new(&skip.pattern).map_err(|e| format!("exclusion {:?}: {e}", skip.pattern))?;
        if let Some(other) = mutants.iter().find(|mutant| pattern.is_match(mutant) && !mutant.starts_with(&own)) {
            stale.push(format!(
                "{}:{}: this `// mutant-skip:` excuses {:?}, which matches a mutant of another line, {other:?}: a \
                 character of the mutant's name that is an operator in a pattern, such as `|`, `(` or `+`, takes a \
                 backslash before it",
                skip.path, skip.line, skip.pattern
            ));
            continue;
        }
        if matches_none(&skip.pattern)? {
            stale.push(format!(
                "{}:{}: this `// mutant-skip:` excuses {:?}, which matches none of the {} mutants listed: the line \
                 under it holds no such mutant; move the comment to directly above the line its mutant is on now, \
                 or remove it",
                skip.path,
                skip.line,
                skip.pattern,
                mutants.len()
            ));
        }
    }
    Ok(stale)
}

/// The patterns of `exclude_re`, each a TOML basic string on a line of its own as the file writes them; an empty list
/// is an error, since that is a file this no longer reads
fn exclude_res(text: &str) -> Result<Vec<String>, String> {
    let (start, close) = exclude_re_list(text)?;
    let mut patterns = Vec::new();
    for line in text[start..close].lines().skip(1).map(str::trim) {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let quoted = line.strip_suffix(',').unwrap_or(line);
        let inner = quoted
            .strip_prefix('"')
            .and_then(|l| l.strip_suffix('"'))
            .ok_or_else(|| format!("exclude_re holds a line this doesn't read: {line}"))?;
        patterns.push(inner.replace("\\\\", "\\").replace("\\\"", "\""));
    }
    if patterns.is_empty() {
        return Err(format!("exclude_re in {CONFIG} holds no pattern this could read"));
    }
    Ok(patterns)
}

/// The engine files, relative to `source`, holding a lock, a condition variable or a thread-local of `std`'s or
/// parking_lot's: their tests, `sync/` itself and the models excepted
fn unmodelled_files(source: &Path) -> Result<std::collections::BTreeSet<String>, String> {
    let mut found = std::collections::BTreeSet::new();
    for (relative, text) in engine_sync_files(source)? {
        if locks_outside_sync(&text) {
            found.insert(relative);
        }
    }
    Ok(found)
}

/// Engine source files scanned by `check-sync`: `sync/`, `tests/` directories, `*tests.rs` and `*model.rs` excepted
fn engine_sync_files(source: &Path) -> Result<Vec<(String, String)>, String> {
    let mut files = Vec::new();
    let mut pending = vec![source.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(&dir).map_err(|e| format!("{}: {e}", dir.display()))? {
            let path = entry.map_err(|e| e.to_string())?.path();
            let relative = path.strip_prefix(source).expect("under source").to_string_lossy().replace('\\', "/");
            if path.is_dir() {
                if relative != "sync" && !relative.ends_with("tests") {
                    pending.push(path);
                }
                continue;
            }
            if !relative.ends_with(".rs") || relative.ends_with("tests.rs") || relative.ends_with("model.rs") {
                continue;
            }
            let text = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
            files.push((relative, text));
        }
    }
    files.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(files)
}

/// Replaces comments (`//`, nested `/* ... */`) and string/char literal bodies with spaces while preserving every
/// newline so byte offsets map 1-to-1 to original line numbers
fn strip_comments_and_strings(source: &str) -> String {
    let bytes = source.as_bytes();
    let len = bytes.len();
    let mut out = String::with_capacity(len);
    let mut i = 0;
    while i < len {
        // Line comment: keep the terminating newline
        if bytes[i] == b'/' && i + 1 < len && bytes[i + 1] == b'/' {
            while i < len && bytes[i] != b'\n' {
                out.push(' ');
                i += 1;
            }
            continue;
        }
        // Nested block comment: keep every newline inside
        if bytes[i] == b'/' && i + 1 < len && bytes[i + 1] == b'*' {
            out.push_str("  ");
            i += 2;
            let mut depth = 1usize;
            while i < len && depth > 0 {
                if bytes[i] == b'/' && i + 1 < len && bytes[i + 1] == b'*' {
                    depth += 1;
                    out.push_str("  ");
                    i += 2;
                } else if bytes[i] == b'*' && i + 1 < len && bytes[i + 1] == b'/' {
                    depth -= 1;
                    out.push_str("  ");
                    i += 2;
                } else {
                    out.push(if bytes[i] == b'\n' { '\n' } else { ' ' });
                    i += 1;
                }
            }
            continue;
        }
        // Raw string literal: r"...", r#"..."#, br#"..."#, cr#"..."#
        let raw_start = if bytes[i] == b'r' {
            Some(i)
        } else if (bytes[i] == b'b' || bytes[i] == b'c') && i + 1 < len && bytes[i + 1] == b'r' {
            Some(i + 1)
        } else {
            None
        };
        if let Some(r_pos) = raw_start {
            let prev_is_ident = i > 0 && (bytes[i - 1].is_ascii_alphanumeric() || bytes[i - 1] == b'_');
            if !prev_is_ident {
                let mut j = r_pos + 1;
                while j < len && bytes[j] == b'#' {
                    j += 1;
                }
                if j < len && bytes[j] == b'"' {
                    let hashes = j - (r_pos + 1);
                    for _ in i..=j {
                        out.push(' ');
                    }
                    i = j + 1;
                    while i < len {
                        if bytes[i] == b'"' && i + hashes < len && bytes[i + 1..=i + hashes].iter().all(|&b| b == b'#')
                        {
                            for _ in 0..=hashes {
                                out.push(' ');
                            }
                            i += hashes + 1;
                            break;
                        }
                        out.push(if bytes[i] == b'\n' { '\n' } else { ' ' });
                        i += 1;
                    }
                    continue;
                }
            }
        }
        // Standard string literal: "..."
        if bytes[i] == b'"' {
            out.push('"');
            i += 1;
            while i < len {
                if bytes[i] == b'\\' && i + 1 < len {
                    out.push(' ');
                    out.push(if bytes[i + 1] == b'\n' { '\n' } else { ' ' });
                    i += 2;
                } else if bytes[i] == b'"' {
                    out.push('"');
                    i += 1;
                    break;
                } else {
                    out.push(if bytes[i] == b'\n' { '\n' } else { ' ' });
                    i += 1;
                }
            }
            continue;
        }
        // Char literal (must not consume Rust lifetimes like `'a` or `'static`)
        if bytes[i] == b'\''
            && let Some(end) = char_literal_end(source, i)
        {
            for b in &bytes[i..end] {
                out.push(if *b == b'\n' { '\n' } else { ' ' });
            }
            i = end;
            continue;
        }
        let ch = source[i..].chars().next().expect("valid utf-8 boundary");
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// If `source[start..]` begins a Rust char literal (`'x'`, `'\''`, `'\n'`, `'\x7f'`, `'\u{1234}'`), returns the byte
/// index just past its closing quote; lifetimes (`'a`, `'static`) return `None`
fn char_literal_end(source: &str, start: usize) -> Option<usize> {
    let rest = &source[start + 1..];
    if let Some(escaped) = rest.strip_prefix('\\') {
        let mut chars = escaped.char_indices();
        let (_, first) = chars.next()?;
        let after_escape = match first {
            'u' => {
                let close = escaped.find('}')?;
                close + 1
            }
            'x' => {
                // \xHH
                let (_, _) = chars.next()?;
                let (idx, ch) = chars.next()?;
                idx + ch.len_utf8()
            }
            _ => first.len_utf8(),
        };
        if escaped[after_escape..].starts_with('\'') {
            return Some(start + 1 + 1 + after_escape + 1);
        }
        None
    } else {
        let mut chars = rest.chars();
        let ch = chars.next()?;
        if ch != '\'' && ch != '\n' && chars.next() == Some('\'') {
            return Some(start + 1 + ch.len_utf8() + 1);
        }
        None
    }
}

/// Expands every `use ...;` statement in comment- and string-stripped Rust code into normalized full paths (such as
/// `std::sync::Mutex`, `std::sync::atomic::AtomicBool`, `std::thread::spawn`), handling multiline braced groups
fn expand_use_trees(code: &str) -> Vec<String> {
    fn split_top_level(text: &str) -> Vec<&str> {
        let mut parts = Vec::new();
        let mut depth = 0usize;
        let mut start = 0;
        for (idx, ch) in text.char_indices() {
            match ch {
                '{' => depth += 1,
                '}' => depth = depth.saturating_sub(1),
                ',' if depth == 0 => {
                    parts.push(&text[start..idx]);
                    start = idx + 1;
                }
                _ => {}
            }
        }
        parts.push(&text[start..]);
        parts
    }

    fn expand(prefix: &str, tree: &str, out: &mut Vec<String>) {
        let tree = tree.trim();
        if tree.is_empty() {
            return;
        }
        if let Some(open) = tree.find('{')
            && let Some(close) = tree.rfind('}')
            && close > open
        {
            let head: String = tree[..open].chars().filter(|c| !c.is_whitespace()).collect();
            let head = head.trim_start_matches("::").trim_end_matches("::");
            let next_prefix = match (prefix.is_empty(), head.is_empty()) {
                (true, _) => head.to_owned(),
                (false, true) => prefix.to_owned(),
                (false, false) => format!("{prefix}::{head}"),
            };
            for item in split_top_level(&tree[open + 1..close]) {
                expand(&next_prefix, item, out);
            }
            return;
        }
        // Strip `as Alias` before collapsing whitespace around `::`
        let path = tree.split(" as ").next().unwrap_or(tree);
        let path: String = path.chars().filter(|c| !c.is_whitespace()).collect();
        let path = path.trim_start_matches("::");
        if path.is_empty() {
            return;
        }
        let full = if path == "self" {
            prefix.to_owned()
        } else if prefix.is_empty() {
            path.to_owned()
        } else {
            format!("{prefix}::{path}")
        };
        if !full.is_empty() {
            out.push(full);
        }
    }

    let use_re = regex::Regex::new(r"\buse\s+([^;]+);").expect("valid regex");
    let mut paths = Vec::new();
    for cap in use_re.captures_iter(code) {
        expand("", &cap[1], &mut paths);
    }
    paths
}

/// The sole module allowed to call `commit_raw()`, relative to `crates/`.
const COMMITS: &str = "elmdb-storage/src/durability.rs";

/// Files implementing or declaring `KvWrite::commit_raw` itself (not call sites).
const COMMIT_RAW_DECLS: &[&str] =
    &["elmdb-storage/src/durability.rs", "elmdb-storage/src/kv.rs", "elmdb-lmdb/src/txn.rs", "elmdb-sim/src/store.rs"];

/// Fails on engine code outside `COMMITS` that calls `.commit_raw()`, naming each line:
/// every write transaction in elmdb v2 must commit exclusively through `DurabilityGate::commit`.
fn check_commits(root: &Path) -> Result<(), String> {
    let crates_dir = root.join("crates");
    let found = commits_outside(&crates_dir)?;
    if found.is_empty() {
        return Ok(());
    }
    Err(format!("these call commit_raw() outside crates/{COMMITS}: {found:?}; commit through DurabilityGate::commit"))
}

/// Lines across `crates/` (tests aside) that invoke `.commit_raw()` outside `COMMIT_RAW_DECLS`.
fn commits_outside(crates_dir: &Path) -> Result<Vec<String>, String> {
    let mut found = Vec::new();
    let mut pending = vec![crates_dir.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(&dir).map_err(|e| format!("{}: {e}", dir.display()))? {
            let path = entry.map_err(|e| e.to_string())?.path();
            let relative = path.strip_prefix(crates_dir).expect("under crates").to_string_lossy().replace('\\', "/");
            if path.is_dir() {
                if !relative.ends_with("/tests") && relative != "tests" {
                    pending.push(path);
                }
                continue;
            }
            if !relative.ends_with(".rs")
                || relative.ends_with("tests.rs")
                || COMMIT_RAW_DECLS.contains(&relative.as_str())
            {
                continue;
            }
            let text = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
            for line in commit_lines(&text) {
                found.push(format!("{relative}:{line}"));
            }
        }
    }
    found.sort();
    Ok(found)
}

/// 1-based line numbers in `source` that invoke `.commit_raw()`.
fn commit_lines(source: &str) -> Vec<usize> {
    let code = strip_comments_and_strings(source);
    let re = regex::Regex::new(r"\.\s*(commit_raw)\s*\(\s*\)").expect("valid regex");
    re.captures_iter(&code)
        .filter_map(|cap| cap.get(1))
        .map(|m| 1 + code[..m.start()].bytes().filter(|&b| b == b'\n').count())
        .collect()
}

/// Whether `source` reaches a lock, a condition variable or a thread-local other than `crate::sync`'s, across both
/// single-line and multiline `use` statements and direct paths
fn locks_outside_sync(source: &str) -> bool {
    let code = strip_comments_and_strings(source);
    let uses = expand_use_trees(&code);
    let std_lock_use = uses.iter().any(|p| {
        matches!(
            p.as_str(),
            "std::sync::Mutex" | "std::sync::Condvar" | "std::sync::RwLock" | "std::sync::Barrier" | "std::sync::*"
        )
    });
    let parking_lot_use = uses.iter().any(|p| p == "parking_lot" || p.starts_with("parking_lot::"));
    let thread_local_use = uses.iter().any(|p| p == "std::thread_local");
    if std_lock_use || parking_lot_use || thread_local_use {
        return true;
    }
    let std_lock_direct = regex::Regex::new(r"\bstd\s*::\s*sync\s*::\s*(?:Mutex|Condvar|RwLock|Barrier)\b")
        .expect("valid regex")
        .is_match(&code);
    let parking_lot_direct = regex::Regex::new(r"\bparking_lot\s*::").expect("valid regex").is_match(&code);
    let thread_local_re = regex::Regex::new(r"\bthread_local\s*!").expect("valid regex");
    let sync_prefix_re = regex::Regex::new(
        r"(?:\bcrate\s*::\s*sync|\belmdb_storage\s*::\s*locks\s*::\s*sync|\bcrate\s*::\s*locks\s*::\s*sync)\s*::\s*$",
    )
    .expect("valid regex");
    let thread_local_direct = thread_local_re.find_iter(&code).any(|m| !sync_prefix_re.is_match(&code[..m.start()]));
    std_lock_direct || parking_lot_direct || thread_local_direct
}

/// `cargo xtask scratch-target`: the scratch target, made, its path printed for a copy to build into with
/// `CARGO_TARGET_DIR`
fn scratch_target(root: &Path) -> Result<(), String> {
    let scratch = build_dir::scratch(&build_dir::target_dir(root)?);
    std::fs::create_dir_all(&scratch).map_err(|e| format!("{}: {e}", scratch.display()))?;
    println!("{}", scratch.display());
    Ok(())
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().expect("xtask sits in the workspace root").to_path_buf()
}

fn generated_dir() -> PathBuf {
    workspace_root().join("crates/elmdb-node/protocol")
}

/// The cargo-about release the notices are written with: another may render the same licenses differently, so
/// `check-notices` would fail on a file nothing changed
const CARGO_ABOUT: &str = "0.9.3";

/// The repository's license files, which the npm package carries copies of beside its .node files
const PACKAGED_LICENSES: [&str; 3] = ["LICENSE-MIT", "LICENSE-APACHE", "LICENSE-OPENLDAP"];

/// The crates' licenses and notices, which cargo-about writes from `about.toml` and `about.hbs`
const CRATE_NOTICES: &str = "THIRD-PARTY-NOTICES";

/// The Rust standard library's notices, the pinned toolchain's own `share/doc/rust/COPYRIGHT-library.html`
const RUST_LIBRARY_NOTICES: &str = "COPYRIGHT-rust-library.html";

/// Everything `notices` writes into the npm package, by file name: the crates' notices, the Rust standard library's,
/// and the copies of the repository's license files. All of it is read before anything is written, so a run that fails
/// leaves the committed files as they were, and the notices' line endings are `\n` whatever the platform's, as
/// `.gitattributes` keeps them in a checkout, since `check-notices` compares bytes.
fn notices(root: &Path) -> Result<Vec<(String, Vec<u8>)>, String> {
    let node = root.join("crates/elmdb-node");
    let read = |path: &Path| std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()));
    let mut files = vec![(CRATE_NOTICES.to_owned(), crate_notices(root, &node)?)];
    let sysroot = Command::new("rustc")
        .args(["--print", "sysroot"])
        .current_dir(root)
        .output()
        .map_err(|e| format!("rustc didn't run: {e}"))?;
    if !sysroot.status.success() {
        return Err(format!("rustc --print sysroot failed:\n{}", String::from_utf8_lossy(&sysroot.stderr)));
    }
    let sysroot = PathBuf::from(String::from_utf8_lossy(&sysroot.stdout).trim());
    let rust_library = read(&sysroot.join("share/doc/rust/COPYRIGHT-library.html"))?;
    files.push((
        RUST_LIBRARY_NOTICES.to_owned(),
        String::from_utf8_lossy(&rust_library).replace("\r\n", "\n").into_bytes(),
    ));
    for name in PACKAGED_LICENSES {
        files.push((name.to_owned(), read(&root.join(name))?));
    }
    Ok(files)
}

/// cargo-about's rendering of `about.hbs`: it reads the binding's dependency tree from the lockfile, without the
/// network, for each target the npm package ships for, so a dependency only one platform builds is listed, with line
/// endings made `\n`, as some crates' license files have `\r\n`. It fails on a dependency whose license it can't
/// establish or that `about.toml` doesn't accept, and so does this on any warning: cargo-about only warns of a
/// clarification whose file no longer matches its checksum, and drops it, which would write lmdb-master-sys's notices
/// without LMDB's license.
fn crate_notices(root: &Path, node: &Path) -> Result<Vec<u8>, String> {
    let version = Command::new(env!("CARGO"))
        .args(["about", "--version"])
        .output()
        .map_err(|e| format!("cargo about didn't run: {e}"))?;
    let version = String::from_utf8_lossy(&version.stdout);
    if version.split_whitespace().last() != Some(CARGO_ABOUT) {
        return Err(format!(
            "The notices are written with cargo-about {CARGO_ABOUT}, and this machine has {}; install it with \
             `cargo install --locked --features cli cargo-about@{CARGO_ABOUT}`",
            if version.trim().is_empty() { "none" } else { version.trim() }
        ));
    }
    let manifest = node.join("package.json");
    let package: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&manifest).map_err(|e| format!("{}: {e}", manifest.display()))?)
            .map_err(|e| format!("{}: {e}", manifest.display()))?;
    let targets: Vec<&str> = package["napi"]["targets"]
        .as_array()
        .ok_or_else(|| format!("{} has no napi.targets", manifest.display()))?
        .iter()
        .map(|target| target.as_str().ok_or_else(|| format!("a napi target in {} isn't a string", manifest.display())))
        .collect::<Result<_, _>>()?;
    let rendered = tempfile::tempdir().map_err(|e| e.to_string())?;
    let out = rendered.path().join(CRATE_NOTICES);
    let mut command = Command::new(env!("CARGO"));
    command.args(["about", "generate", "--frozen", "--fail", "--manifest-path"]);
    command.arg(node.join("Cargo.toml")).arg("--config").arg(node.join("about.toml"));
    for target in targets {
        command.args(["--target", target]);
    }
    let output = command
        .arg("--output-file")
        .arg(&out)
        .arg(node.join("about.hbs"))
        .current_dir(root)
        .output()
        .map_err(|e| format!("cargo about didn't run: {e}"))?;
    let log = String::from_utf8_lossy(&output.stderr);
    if !output.status.success() || log.contains("WARN") || log.contains("ERROR") {
        return Err(format!(
            "cargo about failed or warned; it reads the crates offline, which `cargo fetch --locked` fetches:\n{log}"
        ));
    }
    let text = std::fs::read_to_string(&out).map_err(|e| format!("{}: {e}", out.display()))?;
    Ok(text.replace("\r\n", "\n").into_bytes())
}

fn write_notices(root: &Path) -> Result<(), String> {
    let node = root.join("crates/elmdb-node");
    for (name, contents) in notices(root)? {
        std::fs::write(node.join(&name), contents).map_err(|e| format!("{name}: {e}"))?;
    }
    Ok(())
}

/// Fails when a file `notices` writes differs from the npm package's, or when the package's `files` doesn't list it,
/// which leaves it out of the package
fn check_notices(root: &Path) -> Result<(), String> {
    let node = root.join("crates/elmdb-node");
    let manifest = node.join("package.json");
    let package: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&manifest).map_err(|e| format!("{}: {e}", manifest.display()))?)
            .map_err(|e| format!("{}: {e}", manifest.display()))?;
    let listed: Vec<&str> =
        package["files"].as_array().map(|files| files.iter().filter_map(|f| f.as_str()).collect()).unwrap_or_default();
    let mut stale = Vec::new();
    for (name, contents) in notices(root)? {
        if std::fs::read(node.join(&name)).ok().as_deref() != Some(contents.as_slice()) {
            stale.push(format!("crates/elmdb-node/{name} differs from what `cargo xtask notices` writes"));
        }
        if !listed.contains(&name.as_str()) {
            stale.push(format!("{} doesn't list {name} in its files, so npm leaves it out", manifest.display()));
        }
    }
    if stale.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "The npm package's notices are stale; run `cargo xtask notices`, and list what it writes in package.json's \
             files, and commit:\n  {}",
            stale.join("\n  ")
        ))
    }
}

/// The cargo-deny CI installs (`.github/workflows/ci.yml`, `advisories.yml`), which the install hint names
const CARGO_DENY: &str = "0.21.0";

/// `deny.toml`'s checks, through `cargo`: no banned crate, none from a git repository or a registry other than
/// crates.io, and none with a RustSec advisory `deny.toml` doesn't ignore. The advisories are read from the network.
/// The checks are named, since `deny.toml` leaves licenses to `about.toml` and a bare `cargo deny check` would fail
/// every crate on its license.
fn check_deny(cargo: &str, root: &Path) -> Result<(), String> {
    let version = Command::new(cargo).args(["deny", "--version"]).output().ok().filter(|out| out.status.success());
    let version = version.map(|out| String::from_utf8_lossy(&out.stdout).trim().to_owned());
    if version.as_deref().and_then(|v| v.split_whitespace().last()) != Some(CARGO_DENY) {
        return Err(format!(
            "deny.toml is checked with cargo-deny {CARGO_DENY}, as CI checks it, and this machine has {}: cargo \
             install --locked cargo-deny@{CARGO_DENY}",
            version.as_deref().unwrap_or("none")
        ));
    }
    let deny = |checks: &[&str]| {
        Command::new(cargo)
            .args(["deny", "check"])
            .args(checks)
            .current_dir(root)
            .output()
            .map_err(|e| format!("cargo deny didn't run: {e}"))
    };
    let said = |out: &std::process::Output| {
        format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr))
    };
    let offline = deny(&["bans", "sources"])?;
    if !offline.status.success() {
        return Err(format!("cargo deny check bans sources failed ({}):\n{}", offline.status, said(&offline)));
    }
    // The advisories are read from RustSec's database, which cargo-deny fetches at each run. A machine that can't
    // reach it has read none: that is said and passed over, as what else this machine can't run is, since CI reads
    // them on every change to the lockfile and each day
    let advisories = deny(&["advisories"])?;
    let printed = said(&advisories);
    match advisories.status.success() {
        true => Ok(()),
        false if advisory_database_unreachable(&printed) => {
            eprintln!("  the RustSec advisories weren't read: their database couldn't be fetched. CI reads them");
            Ok(())
        }
        false => Err(format!("cargo deny check advisories failed ({}):\n{printed}", advisories.status)),
    }
}

/// Whether what `cargo deny check advisories` printed says it failed to fetch the database, and found no advisory
fn advisory_database_unreachable(printed: &str) -> bool {
    printed.contains("failed to fetch advisory database") && !printed.contains("error[")
}

fn check_generated() -> Result<(), String> {
    let fresh = tempfile::tempdir().map_err(|e| e.to_string())?;
    generate::write(fresh.path())?;
    let stale = generate::differences(fresh.path(), &generated_dir())?;
    if stale.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "The generated TypeScript is stale; run `cargo xtask generate` and commit the result:\n  {}",
            stale.join("\n  ")
        ))
    }
}

/// The workspace's `Cargo.toml`, the binding's `package.json` and EARS' hold one version, and EARS' peer `elmdb` is
/// exactly it. EARS is built against one binding's protocol and a process holds one binding, so the pair is released
/// at one version, and the peer, an exact version and no range, lets npm install EARS beside that binding alone. The
/// binding's binaries are its platform packages, one per target (`scripts/release-pack.mjs`), packed at its version,
/// so its `optionalDependencies` name each at exactly that version, which napi's loader takes the binary from; and
/// `napi.targets` matches `deny.toml`'s `[graph].targets` and `.github/workflows/release.yml`'s build, SBOM and smoke
/// matrices so adding or removing a target never drifts silently.
fn check_versions(root: &Path) -> Result<(), String> {
    let read = |path: &str| std::fs::read_to_string(root.join(path)).map_err(|e| format!("{path}: {e}"));
    let json = |path: &str| -> Result<serde_json::Value, String> {
        serde_json::from_str(&read(path)?).map_err(|e| format!("{path}: {e}"))
    };
    let binding = json("crates/elmdb-node/package.json")?;
    let mut problems = version_mismatches(&read("Cargo.toml")?, &binding, &json("packages/ears/package.json")?);
    problems.extend(target_parity_mismatches(&binding, &read("deny.toml")?, &read(".github/workflows/release.yml")?));
    match problems.is_empty() {
        true => Ok(()),
        false => Err(format!(
            "The binding and EARS are released as a pair, at the version Cargo.toml's [workspace.package] holds, with \
             one target list across package.json, deny.toml and release.yml:\n  {}",
            problems.join("\n  ")
        )),
    }
}

/// What of the binding's and EARS' manifests doesn't name the workspace's version, each said with the fix
fn version_mismatches(workspace: &str, binding: &serde_json::Value, ears: &serde_json::Value) -> Vec<String> {
    let cargo = table(workspace, "[workspace.package]")
        .find_map(|line| line.strip_prefix("version = \"")?.strip_suffix('"'))
        .map(str::to_owned);
    let Some(cargo) = cargo else {
        return vec!["Cargo.toml's [workspace.package] holds no `version = \"…\"`".to_owned()];
    };
    let held = [
        ("crates/elmdb-node/package.json's `version`", &binding["version"]),
        ("packages/ears/package.json's `version`", &ears["version"]),
        ("packages/ears/package.json's `peerDependencies.elmdb`", &ears["peerDependencies"]["elmdb"]),
    ];
    let mut problems: Vec<String> = held
        .into_iter()
        .filter(|(_, value)| value.as_str() != Some(cargo.as_str()))
        .map(|(what, value)| format!("{what} is {value}, and is to be \"{cargo}\""))
        .collect();
    problems.extend(platform_package_mismatches(&cargo, binding));
    problems
}

/// Each target's platform, which names its package `<napi.packageName>-<platform>` as napi's loader requires it, as
/// `scripts/release-pack.mjs`'s `TARGETS` names them
const PLATFORMS: [(&str, &str); 7] = [
    ("aarch64-apple-darwin", "darwin-arm64"),
    ("x86_64-apple-darwin", "darwin-x64"),
    ("x86_64-unknown-linux-gnu", "linux-x64-gnu"),
    ("aarch64-unknown-linux-gnu", "linux-arm64-gnu"),
    ("x86_64-unknown-linux-musl", "linux-x64-musl"),
    ("aarch64-unknown-linux-musl", "linux-arm64-musl"),
    ("x86_64-pc-windows-msvc", "win32-x64-msvc"),
];

/// What of the binding's `optionalDependencies` doesn't name each of its `napi.targets`' platform package, and only
/// those, at exactly `version`. The platform packages are named in elmdb's own npm scope (`napi.packageName`), where
/// only its owner can publish: an unscoped name not yet on the registry anyone could publish, and an install of elmdb
/// would fetch that stranger's package as its optional dependency and load its binary
fn platform_package_mismatches(version: &str, binding: &serde_json::Value) -> Vec<String> {
    let mut problems = Vec::new();
    let mut expected = Vec::new();
    let prefix = binding["napi"]["packageName"].as_str().filter(|name| name.starts_with("@elmdb/"));
    let Some(prefix) = prefix else {
        return vec![format!(
            "crates/elmdb-node/package.json's `napi.packageName` is {}, and is to name a package in the `@elmdb` scope",
            binding["napi"]["packageName"]
        )];
    };
    for target in binding["napi"]["targets"].as_array().into_iter().flatten() {
        match PLATFORMS.iter().find(|(triple, _)| target.as_str() == Some(triple)) {
            Some((_, platform)) => expected.push(format!("{prefix}-{platform}")),
            None => problems.push(format!(
                "crates/elmdb-node/package.json's `napi.targets` names {target}, which has no platform package"
            )),
        }
    }
    let named = binding["optionalDependencies"].as_object();
    for name in &expected {
        let held = named.and_then(|named| named.get(name)).unwrap_or(&serde_json::Value::Null);
        if held.as_str() != Some(version) {
            problems.push(format!(
                "crates/elmdb-node/package.json's `optionalDependencies.{name}` is {held}, and is to be \"{version}\""
            ));
        }
    }
    for name in named.into_iter().flat_map(|named| named.keys()) {
        if !expected.contains(name) {
            problems.push(format!(
                "crates/elmdb-node/package.json's `optionalDependencies.{name}` is the package of no target in `napi.targets`"
            ));
        }
    }
    problems
}

/// Checks that `crates/elmdb-node/package.json`'s `napi.targets` matches `deny.toml`'s `[graph].targets` and
/// `.github/workflows/release.yml`'s `build` matrix, `SBOMs` loop and `smoke` matrix
fn target_parity_mismatches(binding: &serde_json::Value, deny_toml: &str, release_yml: &str) -> Vec<String> {
    use std::collections::BTreeSet;
    let mut problems = Vec::new();
    let expected_targets: BTreeSet<String> = binding["napi"]["targets"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|t| t.as_str().map(str::to_owned))
        .collect();
    if expected_targets.is_empty() {
        return vec!["crates/elmdb-node/package.json's `napi.targets` holds no targets".to_owned()];
    }
    let expected_pairs: BTreeSet<(String, String)> = expected_targets
        .iter()
        .filter_map(|triple| {
            PLATFORMS.iter().find(|(t, _)| *t == triple).map(|(_, platform)| (triple.clone(), (*platform).to_owned()))
        })
        .collect();
    let expected_platforms: BTreeSet<String> = expected_pairs.iter().map(|(_, p)| p.clone()).collect();

    // 1. deny.toml [graph].targets
    let deny_targets = parse_deny_targets(deny_toml);
    if deny_targets.is_empty() {
        problems.push("deny.toml's [graph].targets holds no targets this could read".to_owned());
    } else {
        for target in expected_targets.difference(&deny_targets) {
            problems.push(format!("deny.toml's [graph].targets is missing {target:?} from `napi.targets`"));
        }
        for target in deny_targets.difference(&expected_targets) {
            problems.push(format!("deny.toml's [graph].targets names {target:?}, which is not in `napi.targets`"));
        }
    }

    // 2. .github/workflows/release.yml build matrix
    let build_pairs = parse_release_build_matrix(release_yml);
    if build_pairs.is_empty() {
        problems.push(".github/workflows/release.yml's `build` matrix holds no targets this could read".to_owned());
    } else {
        for (target, platform) in expected_pairs.difference(&build_pairs) {
            problems.push(format!(
                ".github/workflows/release.yml's `build` matrix is missing ({target}, {platform}) from `napi.targets`"
            ));
        }
        for (target, platform) in build_pairs.difference(&expected_pairs) {
            problems.push(format!(
                ".github/workflows/release.yml's `build` matrix names ({target}, {platform}), which is not in \
                 `napi.targets`"
            ));
        }
    }

    // 3. .github/workflows/release.yml SBOMs step
    let sbom_pairs = parse_release_sboms(release_yml);
    if sbom_pairs.is_empty() {
        problems.push(".github/workflows/release.yml's `SBOMs` step holds no target pairs this could read".to_owned());
    } else {
        for (target, platform) in expected_pairs.difference(&sbom_pairs) {
            problems.push(format!(
                ".github/workflows/release.yml's `SBOMs` step is missing {target}:{platform} from `napi.targets`"
            ));
        }
        for (target, platform) in sbom_pairs.difference(&expected_pairs) {
            problems.push(format!(
                ".github/workflows/release.yml's `SBOMs` step names {target}:{platform}, which is not in `napi.targets`"
            ));
        }
    }

    // 4. .github/workflows/release.yml smoke matrix
    let smoke_platforms = parse_release_smoke_matrix(release_yml);
    if smoke_platforms.is_empty() {
        problems.push(".github/workflows/release.yml's `smoke` matrix holds no platforms this could read".to_owned());
    } else {
        for platform in expected_platforms.difference(&smoke_platforms) {
            problems.push(format!(
                ".github/workflows/release.yml's `smoke` matrix is missing platform {platform:?} from `napi.targets`"
            ));
        }
        for platform in smoke_platforms.difference(&expected_platforms) {
            problems.push(format!(
                ".github/workflows/release.yml's `smoke` matrix names platform {platform:?}, which is not in \
                 `napi.targets`"
            ));
        }
    }

    problems
}

/// Parses `targets = [ ... ]` from `deny.toml`'s `[graph]` table
fn parse_deny_targets(deny_toml: &str) -> std::collections::BTreeSet<String> {
    let quoted = regex::Regex::new(r#""([^"]+)""#).expect("valid regex");
    let mut targets = std::collections::BTreeSet::new();
    let mut in_list = false;
    for raw in table(deny_toml, "[graph]") {
        let line = raw.split('#').next().unwrap_or("").trim();
        if !in_list {
            if let Some(rest) = line.strip_prefix("targets")
                && let Some(after_eq) = rest.trim_start().strip_prefix('=')
                && let Some(after_bracket) = after_eq.trim_start().strip_prefix('[')
            {
                in_list = true;
                for cap in quoted.captures_iter(after_bracket) {
                    targets.insert(cap[1].to_owned());
                }
                if after_bracket.contains(']') {
                    break;
                }
            }
        } else {
            for cap in quoted.captures_iter(line) {
                targets.insert(cap[1].to_owned());
            }
            if line.contains(']') {
                break;
            }
        }
    }
    targets
}

/// Returns the lines of one top-level job under `jobs:` in a GitHub Actions workflow file
fn workflow_job_lines<'a>(workflow: &'a str, job: &str) -> Vec<&'a str> {
    let header = format!("  {job}:");
    let mut lines = workflow.lines().skip_while(|line| line.trim_end() != header);
    if lines.next().is_none() {
        return Vec::new();
    }
    lines
        .take_while(|line| {
            // Next top-level job starts at 2-space indent with a key (not a comment or blank line)
            let is_next_job = line.starts_with("  ")
                && !line.starts_with("   ")
                && !line.trim_start().starts_with('#')
                && !line.trim().is_empty();
            let is_top_level = !line.starts_with(' ') && !line.trim_start().starts_with('#') && !line.trim().is_empty();
            !is_next_job && !is_top_level
        })
        .collect()
}

/// Extracts `(target, platform)` pairs from `.github/workflows/release.yml`'s `build` matrix
fn parse_release_build_matrix(release_yml: &str) -> std::collections::BTreeSet<(String, String)> {
    let target_re = regex::Regex::new(r"\btarget:\s*([A-Za-z0-9_-]+)").expect("valid regex");
    let platform_re = regex::Regex::new(r"\bplatform:\s*([A-Za-z0-9_-]+)").expect("valid regex");
    let mut pairs = std::collections::BTreeSet::new();
    let mut in_include = false;
    for line in workflow_job_lines(release_yml, "build") {
        let code = line.split('#').next().unwrap_or("");
        let trimmed = code.trim();
        if trimmed == "include:" {
            in_include = true;
            continue;
        }
        if in_include {
            if trimmed.is_empty() {
                continue;
            }
            if !trimmed.starts_with('-') {
                break;
            }
            if let (Some(t), Some(p)) = (target_re.captures(trimmed), platform_re.captures(trimmed)) {
                pairs.insert((t[1].to_owned(), p[1].to_owned()));
            }
        }
    }
    pairs
}

/// Extracts `(target, platform)` pairs from `.github/workflows/release.yml`'s `pack` job `SBOMs` step
fn parse_release_sboms(release_yml: &str) -> std::collections::BTreeSet<(String, String)> {
    let pair_re = regex::Regex::new(r"\b([a-z0-9_]+-[a-z0-9_-]+):([a-z0-9_-]+)\b").expect("valid regex");
    let mut pairs = std::collections::BTreeSet::new();
    let mut in_sboms = false;
    let mut in_for = false;
    for line in workflow_job_lines(release_yml, "pack") {
        let trimmed = line.trim();
        if trimmed == "- name: SBOMs" {
            in_sboms = true;
            continue;
        }
        if in_sboms {
            if trimmed.starts_with("- name:") || trimmed.starts_with("- uses:") {
                break;
            }
            if trimmed.starts_with("for pair in ") {
                in_for = true;
            }
            if in_for {
                for cap in pair_re.captures_iter(trimmed) {
                    pairs.insert((cap[1].to_owned(), cap[2].to_owned()));
                }
                if trimmed.contains("; do") {
                    break;
                }
            }
        }
    }
    pairs
}

/// Extracts `platform` names from `.github/workflows/release.yml`'s `smoke` matrix
fn parse_release_smoke_matrix(release_yml: &str) -> std::collections::BTreeSet<String> {
    let platform_re = regex::Regex::new(r"\bplatform:\s*([A-Za-z0-9_-]+)").expect("valid regex");
    let mut platforms = std::collections::BTreeSet::new();
    let mut in_include = false;
    for line in workflow_job_lines(release_yml, "smoke") {
        let code = line.split('#').next().unwrap_or("");
        let trimmed = code.trim();
        if trimmed == "include:" {
            in_include = true;
            continue;
        }
        if in_include {
            if trimmed.is_empty() {
                continue;
            }
            if !trimmed.starts_with('-') {
                break;
            }
            if let Some(p) = platform_re.captures(trimmed) {
                platforms.insert(p[1].to_owned());
            }
        }
    }
    platforms
}

/// Every LMDB `-sys` crate in the build, by name and version. Two copies of LMDB linked into one binary share no locks,
/// and nothing at link time refuses them (heed's `lmdb-master-sys` declares no `links` key), so this does.
fn check_one_lmdb(root: &Path) -> Result<(), String> {
    let tree = dependency_tree(root)?;
    let found = lmdb_sys_crates(&tree);
    match found.len() {
        1 => Ok(()),
        0 => Err("No LMDB -sys crate in the dependency tree; heed's lmdb-master-sys was expected".to_owned()),
        _ => Err(format!(
            "The dependency tree holds {} LMDB -sys crates, and a binary may link only one LMDB:\n  {}",
            found.len(),
            found.join("\n  ")
        )),
    }
}

/// The crates besides ICU's whose output an index stores: charabia's tokens and rust-stemmers' stems in a text index's
/// terms, with what charabia normalizes, detects a language and segments by, which it requires only as ranges.
/// A version that splits or stems differently would leave a stored index that a query no longer matches.
const INDEXED: [&str; 5] = ["charabia", "finl_unicode", "rust-stemmers", "unicode-normalization", "whatlang"];

/// Every ICU crate the lockfile holds, and each of `INDEXED`, required by the workspace at exactly its locked version
/// and named among `elmdb`'s dependencies. A sort key is the collator's output: one stored under the stamp of these
/// versions (`engine/order.rs`) is read as this collator's, so a build that resolved another version of any of them
/// would read stale keys with no error; an index holds the others' output the same way. Required exactly, a consumer
/// of the crate resolves these or fails to resolve
fn check_exact(root: &Path) -> Result<(), String> {
    let read = |path: &str| std::fs::read_to_string(root.join(path)).map_err(|e| format!("{path}: {e}"));
    let (lock, workspace, elmdb) = (read("Cargo.lock")?, read("Cargo.toml")?, read("crates/elmdb/Cargo.toml")?);
    let mut problems = Vec::new();
    let mut indexed_found = Vec::new();
    let mut lines = lock.lines();
    while let Some(line) = lines.next() {
        let Some(name) = line.strip_prefix("name = \"").and_then(|rest| rest.strip_suffix('"')) else { continue };
        if INDEXED.contains(&name) {
            indexed_found.push(name);
        } else if !name.starts_with("icu_") {
            continue;
        }
        let version = lines.next().and_then(|line| line.strip_prefix("version = \"")).unwrap_or_default();
        let version = version.trim_end_matches('"');
        // From the registry, where a version is one thing: a path or a git source under the version is another crate
        if !lines.next().is_some_and(|line| line.starts_with("source = \"registry+")) {
            problems.push(format!("Cargo.lock doesn't take {name} from the registry"));
        }
        // Each in the table a build of the crate reads: one under a table of development or a target's
        // dependencies would hold no one who depends on elmdb
        let required =
            table(&workspace, "[workspace.dependencies]").find(|line| line.starts_with(&format!("{name} = ")));
        if !required.is_some_and(|line| line.contains(&format!("\"={version}\""))) {
            problems.push(format!("Cargo.toml's [workspace.dependencies] doesn't require {name} at exactly {version}"));
        }
        if !table(&elmdb, "[dependencies]").any(|line| line.trim() == format!("{name}.workspace = true")) {
            problems.push(format!("crates/elmdb/Cargo.toml's [dependencies] doesn't name {name}"));
        }
    }
    // A name here that the lockfile no longer holds would be checked by nothing
    for name in INDEXED {
        if !indexed_found.contains(&name) {
            problems.push(format!("Cargo.lock holds no {name}, which INDEXED names"));
        }
    }
    match problems.is_empty() {
        true => Ok(()),
        false => Err(format!(
            "A crate the collator is built from, or whose output an index stores, isn't required exactly, so a \
             build may resolve another and read what is stored as its own:\n  {}",
            problems.join("\n  ")
        )),
    }
}

/// The lines of one table of a manifest: those after its header, up to the next table's
fn table<'a>(manifest: &'a str, header: &'a str) -> impl Iterator<Item = &'a str> {
    manifest.lines().skip_while(move |line| line.trim() != header).skip(1).take_while(|line| !line.starts_with('['))
}

/// The workspace's crates at their default features and what they build with, one `<name> v<version>` a line;
/// development dependencies are left out, being in no build a user gets
fn dependency_tree(root: &Path) -> Result<String, String> {
    let output = Command::new(env!("CARGO"))
        .args(["tree", "--workspace", "--edges", "normal,build", "--prefix", "none", "--format", "{p}"])
        .current_dir(root)
        .output()
        .map_err(|e| format!("cargo tree didn't run: {e}"))?;
    if !output.status.success() {
        return Err(format!("cargo tree failed:\n{}", String::from_utf8_lossy(&output.stderr)));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Diagnostics are off by default (`crates/elmdb/src/diagnostics.rs`): without the `diagnostics` feature `elmdb` does
/// not depend on `tracing`, so its spans compile to nothing. `tracing` is in the build regardless, through hannoy, whose
/// own spans are its own; what this holds is that no workspace crate turns elmdb's on by default
fn check_diagnostics_off(root: &Path) -> Result<(), String> {
    let output = Command::new(env!("CARGO"))
        .args(["tree", "--workspace", "--edges", "normal,build", "--prefix", "depth", "--format", "{p}"])
        .current_dir(root)
        .output()
        .map_err(|e| format!("cargo tree didn't run: {e}"))?;
    if !output.status.success() {
        return Err(format!("cargo tree failed:\n{}", String::from_utf8_lossy(&output.stderr)));
    }
    let tree = String::from_utf8_lossy(&output.stdout);
    let direct = direct_dependencies(&tree, "elmdb");
    if direct.is_empty() {
        return Err(format!("cargo tree listed no dependency of elmdb, so this checked nothing:\n{tree}"));
    }
    match direct.iter().find(|name| name.as_str() == "tracing") {
        None => Ok(()),
        Some(_) => Err("A default build has elmdb depend on tracing, which only its `diagnostics` feature may: a \
                        workspace crate enables that feature by default"
            .to_owned()),
    }
}

/// Failpoints are off by default (`crates/elmdb-storage/src/failpoints.rs`): neither default workspace builds nor
/// `elmdb-node` (npm/release) builds may enable `failpoints` via feature unification.
fn check_failpoints_off(root: &Path) -> Result<(), String> {
    for scope in [&["--workspace"][..], &["-p", "elmdb-node"][..]] {
        let output = Command::new(env!("CARGO"))
            .arg("tree")
            .args(scope)
            .args([
                "--target",
                "all",
                "--no-dedupe",
                "--edges",
                "normal,build",
                "--prefix",
                "none",
                "--format",
                "{p}|{f}",
            ])
            .current_dir(root)
            .output()
            .map_err(|e| format!("cargo tree didn't run: {e}"))?;
        if !output.status.success() {
            return Err(format!("cargo tree failed:\n{}", String::from_utf8_lossy(&output.stderr)));
        }
        let tree = String::from_utf8_lossy(&output.stdout);
        verify_no_failpoints_in_tree(&tree)?;
    }
    Ok(())
}

fn verify_no_failpoints_in_tree(tree: &str) -> Result<(), String> {
    let mut saw_storage = false;
    let mut violations = Vec::new();
    for line in tree.lines() {
        let Some((pkg_part, feats_part)) = line.split_once('|') else { continue };
        let Some(pkg) = pkg_part.split_whitespace().next() else { continue };
        if pkg == "elmdb-storage" {
            saw_storage = true;
        }
        let cleaned_feats = feats_part.trim().strip_suffix(" (*)").unwrap_or(feats_part.trim());
        if matches!(pkg, "elmdb-storage" | "elmdb-lmdb" | "elmdb")
            && cleaned_feats.split(',').map(str::trim).any(|f| f == "failpoints")
        {
            violations.push(format!("{pkg} ({cleaned_feats})"));
        }
    }
    if !saw_storage {
        return Err(format!("cargo tree listed no elmdb-storage, so check-failpoints-off checked nothing:\n{tree}"));
    }
    if violations.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "default build enabled `failpoints` via feature unification: {violations:?}; failpoints must remain opt-in"
        ))
    }
}

fn check_leiden_serial(root: &Path) -> Result<(), String> {
    let output = Command::new(env!("CARGO"))
        .args(["tree", "--workspace", "--edges", "features", "--invert", "leiden-rs", "--depth", "0"])
        .args(["--prefix", "none", "--format", "{p}|{f}"])
        .current_dir(root)
        .output()
        .map_err(|e| format!("cargo tree didn't run: {e}"))?;
    if !output.status.success() {
        return Err(format!("cargo tree failed:\n{}", String::from_utf8_lossy(&output.stderr)));
    }
    let tree = String::from_utf8_lossy(&output.stdout);
    let Some(features) = tree.lines().find_map(|line| line.strip_prefix("leiden-rs ")) else {
        return Err(format!("cargo tree listed no leiden-rs, so this checked nothing:\n{tree}"));
    };
    match features.split_once('|').map(|(_, f)| f.trim()) {
        Some("") => Ok(()),
        enabled => Err(format!(
            "The workspace builds leiden-rs with features {enabled:?}; communities need it with none, its rayon \
             feature making a partition depend on thread timing"
        )),
    }
}

/// The names of `package`'s direct dependencies in a `cargo tree --prefix depth` listing of the workspace
fn direct_dependencies(tree: &str, package: &str) -> Vec<String> {
    let mut under = false;
    let mut found = Vec::new();
    for line in tree.lines() {
        if let Some(root) = line.strip_prefix('0') {
            under = root.split_whitespace().next() == Some(package);
        } else if under && let Some(child) = line.strip_prefix('1') {
            found.extend(child.split_whitespace().next().map(str::to_owned));
        }
    }
    found
}

/// The distinct `<name> v<version>` entries of a `cargo tree` listing whose crate is an LMDB `-sys` crate
fn lmdb_sys_crates(tree: &str) -> Vec<String> {
    let mut found: Vec<String> = tree
        .lines()
        .filter_map(|line| {
            let mut words = line.split_whitespace();
            let name = words.next()?;
            let version = words.next()?;
            (name.contains("lmdb") && name.ends_with("-sys")).then(|| format!("{name} {version}"))
        })
        .collect();
    found.sort();
    found.dedup();
    found
}

#[cfg(test)]
mod tests {
    use super::{Skip, exclude_res, skips_in, stale_exclusions, with_skips};
    use super::{
        UNMODELLED, check_workspace_deps, direct_dependencies, lmdb_sys_crates, locks_outside_sync, unmodelled_files,
    };
    use super::{
        check_deny, platform_package_mismatches, target_parity_mismatches, version_mismatches, workspace_root,
    };

    #[test]
    fn the_pair_is_held_to_the_workspace_version_and_the_peer_to_exactly_it() {
        let workspace = "[workspace]\nmembers = []\n\n[workspace.package]\nedition = \"2024\"\nversion = \"0.1.0\"\n\n[x]\nversion = \"9.9.9\"\n";
        let binding = serde_json::json!({ "version": "0.1.0", "napi": { "packageName": "@elmdb/binding" } });
        let ears = |version: &str, peer: serde_json::Value| serde_json::json!({ "version": version, "peerDependencies": { "elmdb": peer } });
        assert_eq!(version_mismatches(workspace, &binding, &ears("0.1.0", "0.1.0".into())), Vec::<String>::new());
        // Each of the three told apart, a range is not the version, and a missing peer is named
        let mismatches = version_mismatches(
            workspace,
            &serde_json::json!({ "version": "0.2.0", "napi": { "packageName": "@elmdb/binding" } }),
            &ears("0.1.1", "^0.1.0".into()),
        );
        assert_eq!(mismatches.len(), 3, "{mismatches:?}");
        assert!(mismatches[0].starts_with("crates/elmdb-node/package.json's `version` is \"0.2.0\""), "{mismatches:?}");
        assert!(mismatches[1].starts_with("packages/ears/package.json's `version` is \"0.1.1\""), "{mismatches:?}");
        assert!(mismatches[2].contains("`peerDependencies.elmdb` is \"^0.1.0\""), "{mismatches:?}");
        let missing = version_mismatches(workspace, &binding, &serde_json::json!({ "version": "0.1.0" }));
        assert_eq!(missing, ["packages/ears/package.json's `peerDependencies.elmdb` is null, and is to be \"0.1.0\""]);
        // A workspace with no version of its own is refused, not read as matching nothing
        let unversioned =
            version_mismatches("[workspace.package]\nedition = \"2024\"\n", &binding, &ears("0.1.0", "0.1.0".into()));
        assert_eq!(unversioned.len(), 1);
        assert!(unversioned[0].contains("holds no `version"), "{unversioned:?}");
    }

    #[test]
    fn each_target_has_its_platform_package_at_exactly_the_version_and_nothing_else_is_named() {
        let binding = |targets: serde_json::Value, optional: serde_json::Value| serde_json::json!({ "napi": { "packageName": "@elmdb/binding", "targets": targets }, "optionalDependencies": optional });
        let targets = serde_json::json!(["aarch64-apple-darwin", "x86_64-unknown-linux-musl"]);
        let both =
            serde_json::json!({ "@elmdb/binding-darwin-arm64": "0.1.0", "@elmdb/binding-linux-x64-musl": "0.1.0" });
        assert_eq!(platform_package_mismatches("0.1.0", &binding(targets.clone(), both)), Vec::<String>::new());
        // A range, a missing package, one no target builds and a target with no platform package are each named
        let wrong =
            serde_json::json!({ "@elmdb/binding-darwin-arm64": "^0.1.0", "@elmdb/binding-linux-x64-gnu": "0.1.0" });
        let problems = platform_package_mismatches("0.1.0", &binding(targets, wrong));
        assert_eq!(problems.len(), 3, "{problems:?}");
        assert!(
            problems[0].contains("`optionalDependencies.@elmdb/binding-darwin-arm64` is \"^0.1.0\""),
            "{problems:?}"
        );
        assert!(problems[1].contains("`optionalDependencies.@elmdb/binding-linux-x64-musl` is null"), "{problems:?}");
        assert!(problems[2].contains("@elmdb/binding-linux-x64-gnu` is the package of no target"), "{problems:?}");
        let unknown = platform_package_mismatches(
            "0.1.0",
            &binding(serde_json::json!(["riscv64gc-unknown-linux-gnu"]), serde_json::json!({})),
        );
        assert_eq!(unknown.len(), 1);
        assert!(unknown[0].contains("riscv64gc-unknown-linux-gnu"), "{unknown:?}");
        // No optional dependencies at all is every target's package missing
        let none = platform_package_mismatches(
            "0.1.0",
            &serde_json::json!({ "napi": { "packageName": "@elmdb/binding", "targets": ["x86_64-apple-darwin"] } }),
        );
        assert_eq!(none.len(), 1, "{none:?}");
        // Platform packages outside elmdb's scope, or with no name given, which napi takes from the package's own, are
        // refused whatever the optional dependencies hold
        for name in [serde_json::json!("elmdb"), serde_json::Value::Null, serde_json::json!("@other/binding")] {
            let unscoped = serde_json::json!({ "napi": { "packageName": name, "targets": ["x86_64-apple-darwin"] },
                "optionalDependencies": { "elmdb-darwin-x64": "0.1.0" } });
            let refused = platform_package_mismatches("0.1.0", &unscoped);
            assert_eq!(refused.len(), 1, "{refused:?}");
            assert!(refused[0].contains("`napi.packageName`"), "{refused:?}");
        }
    }

    #[test]
    fn the_checkout_holds_one_version() {
        super::check_versions(&workspace_root()).unwrap();
    }

    #[test]
    fn without_cargo_deny_the_check_refuses_and_says_how_to_install_it() {
        // A program that isn't there, and, where the platform has a `false`, one that is and has no `deny` to run
        let missing = workspace_root().join("target/no-such-cargo");
        for cargo in [missing.to_str().unwrap(), "false"] {
            let refused = check_deny(cargo, &workspace_root()).unwrap_err();
            assert!(refused.contains("cargo install --locked cargo-deny@0.21.0"), "{refused}");
        }
    }

    #[test]
    fn advisories_that_couldnt_be_fetched_are_told_from_one_that_was_found() {
        assert!(super::advisory_database_unreachable("error: failed to fetch advisory database https://x\n"));
        assert!(!super::advisory_database_unreachable("error[unmaintained]: Bincode is unmaintained\n"));
        // A database that was fetched before and an advisory read from it: the advisory still fails
        assert!(!super::advisory_database_unreachable(
            "warning: failed to fetch advisory database\nerror[vulnerability]: x\n"
        ));
    }

    fn skip(line: usize, pattern: &str) -> Skip {
        Skip { path: "crates/x/src/a.rs".to_owned(), line, pattern: pattern.to_owned() }
    }

    #[test]
    fn a_mutant_skip_names_the_line_of_the_code_after_it_and_its_reason_stays_in_the_source() {
        let source = "\
fn a() {
    // mutant-skip: replace > with >= in a
    // Its reason, over
    //
    // more than one line
    if x > 1 {}

    // mutant-skip: replace \\+ with \\* in a (as above)
    y + 1;
    // not a skip: mutant-skip: nothing
}
";
        assert_eq!(
            skips_in("crates/x/src/a.rs", source).unwrap(),
            [
                skip(2, "crates/x/src/a.rs:6:.*replace > with >= in a"),
                skip(8, "crates/x/src/a.rs:9:.*replace \\+ with \\* in a"),
            ]
        );
        assert!(skips_in("a.rs", "// mutant-skip: x\n// and nothing after\n").is_err());
    }

    #[test]
    fn the_configuration_a_run_reads_is_the_committed_one_with_each_comments_exclusion() {
        let committed = "exclude_globs = [\n    \"a/**\",\n]\nexclude_re = [\n    # why\n    \"kept\",\n]\nlast = 1\n";
        let derived =
            with_skips(committed, &[skip(2, "a.rs:6:.*replace \\+ with \\* in a"), skip(8, "a.rs:9:\"")]).unwrap();
        // The committed text whole, after a heading that says what wrote it, with the comments' patterns added
        let (heading, rest) = derived.split_once("\n\n").unwrap();
        assert!(heading.lines().all(|line| line.starts_with("# ")), "{heading}");
        assert_eq!(
            rest,
            "exclude_globs = [\n    \"a/**\",\n]\nexclude_re = [\n    # why\n    \"kept\",\n    \
             \"a.rs:6:.*replace \\\\+ with \\\\* in a\",\n    \"a.rs:9:\\\"\",\n]\nlast = 1\n"
        );
        // And they read back as the patterns they were, so cargo-mutants excludes what the comments name
        assert_eq!(exclude_res(&derived).unwrap(), ["kept", "a.rs:6:.*replace \\+ with \\* in a", "a.rs:9:\""]);
        assert_eq!(with_skips(committed, &[]).unwrap(), format!("{heading}\n\n{committed}"));
        assert!(with_skips("exclude_globs = []\n", &[]).is_err());
        // A last pattern without its comma would run into the first one added
        let refused = with_skips("a = 1\nexclude_re = [\n    \"kept\"\n    # why\n]\n", &[]).unwrap_err();
        assert!(refused.contains("no comma"), "{refused}");
        // A committed pattern that names a line makes no configuration, whatever the comments are
        for named in ["crates/x/src/a.rs:6:.*replace > with >= in a", "a.rs:(6|9):.*replace"] {
            let kept = format!("a = 1\nexclude_re = [\n    \"kept.rs:.*replace 1\",\n    \"{named}\",\n]\n");
            let refused = with_skips(&kept, &[skip(2, "a.rs:6:x")]).unwrap_err();
            assert!(refused.contains("names a line") && refused.contains(named), "{refused}");
        }
    }

    #[test]
    fn an_exclusion_that_names_no_mutant_is_stale_and_a_committed_one_names_no_line() {
        let mutants = [
            "crates/x/src/a.rs:6:10: replace > with >= in a",
            "crates/x/src/a.rs:9:7: replace + with * in a",
            "crates/x/src/b.rs:3:5: replace b -> bool with true",
        ];
        let committed = ["b.rs:.*replace b -> bool with".to_owned()];
        let skips = [skip(2, "crates/x/src/a.rs:6:.*replace > with >= in a")];
        assert_eq!(stale_exclusions(&committed, &skips, &mutants).unwrap(), [""; 0]);

        // The comment's mutant is on another line than the one under the comment, as after code went in between
        let moved = stale_exclusions(&committed, &[skip(2, "crates/x/src/a.rs:5:.*replace > with >= in a")], &mutants);
        let moved = moved.unwrap();
        assert_eq!(moved.len(), 1, "{moved:?}");
        assert!(moved[0].starts_with("crates/x/src/a.rs:2: ") && moved[0].contains("matches none"), "{moved:?}");
        // The line under the comment holds a mutant, of another kind than the comment names
        let other = stale_exclusions(&committed, &[skip(8, "crates/x/src/a.rs:9:.*replace > with >= in a")], &mutants);
        assert_eq!(other.unwrap().len(), 1);

        // A committed pattern whose mutant is gone
        let gone = stale_exclusions(&["b.rs:.*replace b -> u8 with".to_owned()], &skips, &mutants).unwrap();
        assert!(gone.len() == 1 && gone[0].contains("matches none"), "{gone:?}");
        // A committed pattern that names a line is refused though it matches, one line or several
        for pattern in ["crates/x/src/a.rs:6:.*replace > with >= in a", "a.rs:(6|9):.*replace"] {
            let named = stale_exclusions(&[pattern.to_owned()], &[], &mutants).unwrap();
            assert!(named.len() == 1 && named[0].contains("names a line"), "{named:?}");
        }
        assert!(stale_exclusions(&["(".to_owned()], &[], &mutants).is_err());
        // A comment whose mutant's name holds `||` unescaped is a pattern with nothing on one side of a `|`, which
        // matches every mutant: refused for the mutants of other lines it excuses, and taken once it is escaped
        let ors =
            ["crates/x/src/a.rs:6:10: replace && with || in a", "crates/x/src/b.rs:3:5: replace b -> bool with true"];
        let all = stale_exclusions(&[], &[skip(5, "crates/x/src/a.rs:6:.*replace && with || in a")], &ors).unwrap();
        assert!(all.len() == 1 && all[0].contains("another line"), "{all:?}");
        let one = stale_exclusions(&[], &[skip(5, "crates/x/src/a.rs:6:.*replace && with \\|\\| in a")], &ors);
        assert_eq!(one.unwrap(), [""; 0]);
    }

    #[test]
    fn a_lock_outside_crate_sync_is_found_and_one_through_it_is_not() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path();
        std::fs::create_dir_all(src.join("engine/tests")).unwrap();
        std::fs::create_dir_all(src.join("sync")).unwrap();
        std::fs::write(src.join("engine/locked.rs"), "use std::sync::{Arc, Mutex};\n").unwrap();
        std::fs::write(src.join("engine/multiline.rs"), "use std::sync::{\n    Arc,\n    Condvar,\n};\n").unwrap();
        std::fs::write(src.join("engine/nested.rs"), "use std::{\n    sync::{\n        RwLock,\n    },\n};\n").unwrap();
        std::fs::write(src.join("engine/parked.rs"), "fn f(_: parking_lot::Mutex<()>) {}\n").unwrap();
        std::fs::write(src.join("engine/local.rs"), "thread_local! { static X: u8 = 0; }\n").unwrap();
        std::fs::write(
            src.join("engine/modelled.rs"),
            "use elmdb_storage::locks::sync::{Mutex, Condvar};\nelmdb_storage::locks::sync::thread_local! {}\n",
        )
        .unwrap();
        std::fs::write(
            src.join("engine/commented.rs"),
            "// std::sync::Mutex is not used here\n/* std::sync::Condvar */\nconst S: &str = \"std::sync::Mutex\";\nuse std::sync::Arc;\n",
        )
        .unwrap();
        std::fs::write(src.join("engine/tests/uses.rs"), "use std::sync::Mutex;\n").unwrap();
        std::fs::write(src.join("sync/mod.rs"), "pub(crate) use std::sync::{Condvar, Mutex};\n").unwrap();
        let found: Vec<String> = unmodelled_files(src).unwrap().into_iter().collect();
        assert_eq!(
            found,
            ["engine/local.rs", "engine/locked.rs", "engine/multiline.rs", "engine/nested.rs", "engine/parked.rs"]
        );
        assert!(!locks_outside_sync("    // parking_lot::Mutex"));
        assert!(!locks_outside_sync("fn f<'a>(_: &'a str) {}"));
    }

    #[test]
    fn check_workspace_deps_passes_when_all_used_and_flags_dead_entries() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("crates/a")).unwrap();
        std::fs::write(
            root.join("Cargo.toml"),
            r#"
[workspace]
members = [
    'crates/a',
]

[workspace.dependencies]
alpha = "1.0"
beta = {
    version = "2.0",
    features = ["std"],
}
gamma.version = "3.0"
libc = "0.2"

[workspace.dependencies.delta]
version = "4.0"

[workspace.dependencies.epsilon]
version = "5.0"
"#,
        )
        .unwrap();
        std::fs::write(
            root.join("crates/a/Cargo.toml"),
            r#"
[package]
name = "a"

[dependencies]
alpha.workspace = true
beta = {
    optional = true,
    workspace = true,
}

[dependencies.gamma]
workspace = true

[target.'cfg(unix)'.dependencies]
libc = {
    features = ["extra_traits"],
    workspace = true,
}

[target.'cfg(windows)'.dev-dependencies.delta]
features = ["Win32"]
workspace = true

[build-dependencies.epsilon]
workspace = true
"#,
        )
        .unwrap();
        check_workspace_deps(root).unwrap();

        // Adding an unused workspace dependency (inline or subtable) fails cleanly and names it.
        std::fs::write(
            root.join("Cargo.toml"),
            r#"
[workspace]
members = ["crates/a"]

[workspace.dependencies]
alpha = "1.0"
beta = "2.0"
gamma = "3.0"
libc = "0.2"
delta = "4.0"
epsilon = "5.0"
dead-crate = "0.1"

[workspace.dependencies.dead-subtable]
version = "0.2"
"#,
        )
        .unwrap();
        let err = check_workspace_deps(root).unwrap_err();
        assert!(err.contains("dead-crate"), "{err}");
        assert!(err.contains("dead-subtable"), "{err}");
    }

    #[test]
    fn a_commit_outside_the_one_module_is_found_and_one_in_it_or_in_a_test_is_not() {
        let dir = tempfile::tempdir().unwrap();
        let storage_src = dir.path().join("crates/elmdb-storage/src");
        let engine_src = dir.path().join("crates/elmdb-engine/src");
        std::fs::create_dir_all(&storage_src).unwrap();
        std::fs::create_dir_all(engine_src.join("tests")).unwrap();
        std::fs::write(storage_src.join("durability.rs"), "fn commit(txn: RwTxn) { txn.commit_raw() }\n").unwrap();
        std::fs::write(engine_src.join("tests.rs"), "fn t(txn: RwTxn) { txn.commit_raw().unwrap(); }\n").unwrap();
        std::fs::write(engine_src.join("tests/uses.rs"), "fn t(txn: RwTxn) { txn.commit_raw().unwrap(); }\n").unwrap();
        let ok = "fn f(txn: GatedWriteTxn) {\n    // txn.commit_raw() is gated\n    let _ = \"txn.commit_raw()\";\n    txn.commit();\n}\n";
        std::fs::write(engine_src.join("commit.rs"), ok).unwrap();
        assert_eq!(super::check_commits(dir.path()), Ok(()));

        std::fs::write(engine_src.join("bad.rs"), "fn f(txn: RwTxn) {\n\n    txn.commit_raw()?;\n}\n").unwrap();
        let multi = "fn f(txn: RwTxn) {\n    txn\n        .commit_raw()?;\n    txn\n        .\n        commit_raw(\n            /* none */\n        )?;\n}\n";
        std::fs::write(engine_src.join("multi.rs"), multi).unwrap();
        let refused = super::check_commits(dir.path()).unwrap_err();
        assert!(
            refused.contains(
                r#"["elmdb-engine/src/bad.rs:3", "elmdb-engine/src/multi.rs:3", "elmdb-engine/src/multi.rs:6"]"#
            ),
            "{refused}"
        );
        assert!(refused.contains("crates/elmdb-storage/src/durability.rs"), "{refused}");
    }

    #[test]
    fn targets_must_match_across_package_json_deny_toml_and_release_yml() {
        let binding = serde_json::json!({
            "napi": {
                "packageName": "@elmdb/binding",
                "targets": ["aarch64-apple-darwin", "x86_64-unknown-linux-gnu"]
            }
        });
        let deny = "[graph]\ntargets = [\n    \"aarch64-apple-darwin\",\n    \"x86_64-unknown-linux-gnu\",\n]\n";
        let release = "\
jobs:
  build:
    strategy:
      matrix:
        include:
          - { target: aarch64-apple-darwin, runner: macos-15, platform: darwin-arm64 }
          - { target: x86_64-unknown-linux-gnu, runner: ubuntu-24.04, platform: linux-x64-gnu }
  pack:
    steps:
      - name: SBOMs
        run: |
          for pair in aarch64-apple-darwin:darwin-arm64 x86_64-unknown-linux-gnu:linux-x64-gnu; do
            echo $pair
          done
  smoke:
    strategy:
      matrix:
        include:
          - { platform: darwin-arm64, runner: macos-15 }
          - { platform: linux-x64-gnu, runner: ubuntu-24.04 }
";
        assert_eq!(target_parity_mismatches(&binding, deny, release), Vec::<String>::new());

        // Missing target in deny.toml and extra target in deny.toml
        let bad_deny = "[graph]\ntargets = [\"aarch64-apple-darwin\", \"x86_64-pc-windows-msvc\"]\n";
        let probs = target_parity_mismatches(&binding, bad_deny, release);
        assert_eq!(probs.len(), 2, "{probs:?}");
        assert!(
            probs[0].contains("deny.toml")
                && probs[0].contains("missing")
                && probs[0].contains("x86_64-unknown-linux-gnu")
        );
        assert!(probs[1].contains("deny.toml") && probs[1].contains("x86_64-pc-windows-msvc"));

        // Drift in release.yml build, SBOMs, and smoke
        let bad_release = "\
jobs:
  build:
    strategy:
      matrix:
        include:
          - { target: aarch64-apple-darwin, runner: macos-15, platform: darwin-arm64 }
  pack:
    steps:
      - name: SBOMs
        run: |
          for pair in aarch64-apple-darwin:darwin-arm64; do
            echo $pair
          done
  smoke:
    strategy:
      matrix:
        include:
          - { platform: darwin-arm64, runner: macos-15 }
          - { platform: linux-x64-gnu, runner: ubuntu-24.04 }
          - { platform: win32-x64-msvc, runner: windows-2022 }
";
        let probs = target_parity_mismatches(&binding, deny, bad_release);
        assert_eq!(probs.len(), 3, "{probs:?}");
        assert!(probs[0].contains("`build` matrix") && probs[0].contains("x86_64-unknown-linux-gnu"));
        assert!(probs[1].contains("`SBOMs` step") && probs[1].contains("x86_64-unknown-linux-gnu:linux-x64-gnu"));
        assert!(probs[2].contains("`smoke` matrix") && probs[2].contains("win32-x64-msvc"));
    }

    #[test]
    fn a_listed_file_that_no_longer_locks_is_stale() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("crates/elmdb/src")).unwrap();
        let refused = super::check_sync(dir.path()).unwrap_err();
        assert!(
            refused.contains("no longer lock")
                && refused.contains("diagnostics.rs")
                && refused.contains("env.rs")
                && !refused.contains("db.rs"),
            "{refused}"
        );
    }

    #[test]
    fn every_unmodelled_file_says_why() {
        assert!(UNMODELLED.iter().all(|(file, why)| file.ends_with(".rs") && !why.is_empty()));
    }

    #[test]
    fn names_only_the_packages_own_direct_dependencies() {
        let tree = "0elmdb v0.0.0 (/x)\n1hannoy v0.3.0\n2tracing v0.1.44\n1heed v0.23.0\n0elmdb-node v0.0.0 (/y)\n1tracing v0.1.44\n";
        assert_eq!(direct_dependencies(tree, "elmdb"), ["hannoy", "heed"]);
        assert_eq!(direct_dependencies(tree, "elmdb-node"), ["tracing"]);
        assert!(direct_dependencies(tree, "elmdb-cli").is_empty());
    }

    #[test]
    fn finds_each_lmdb_sys_crate_once() {
        let tree = "elmdb v0.0.0 (/x)\nheed v0.23.0\nlmdb-master-sys v0.2.6\nlmdb-master-sys v0.2.6 (*)\nlibc v0.2.1\n";
        assert_eq!(lmdb_sys_crates(tree), ["lmdb-master-sys v0.2.6"]);
    }

    #[test]
    fn counts_two_versions_and_two_crates_apart() {
        let tree = "lmdb-master-sys v0.2.6\nlmdb-master-sys v0.2.5\nlmdb-master3-sys v0.2.6\nlmdb-rkv v0.14.0\n";
        assert_eq!(lmdb_sys_crates(tree).len(), 3);
    }

    #[test]
    fn failpoints_off_check_rejects_unified_failpoints_feature_and_empty_tree() {
        assert!(super::verify_no_failpoints_in_tree("elmdb v0.1.0|\nelmdb-storage v0.1.0|\n").is_ok());
        assert!(super::verify_no_failpoints_in_tree("other v0.1.0|\n").is_err());
        let err = super::verify_no_failpoints_in_tree("elmdb v0.1.0|\nelmdb-storage v0.1.0|failpoints\n").unwrap_err();
        assert!(err.contains("elmdb-storage"), "{err}");
    }
}
