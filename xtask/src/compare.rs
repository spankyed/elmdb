//! `cargo xtask compare`: one command on two revisions, in turn and each run in the other order, each in a worktree of its
//! own kept under `target/compare/` so its builds stay warm, and every reading beside the machine's load as its run ended. It
//! reports and decides nothing: alternating is what keeps a spike or a drift in load from reading as a change, and whether a
//! run taken under one counts is the reader's call.

use std::path::Path;
use std::process::Command;

const USAGE: &str = "usage: cargo xtask compare <base> [--head <rev>] [--runs <n>] -- <command> [arg]...";

/// What was asked for
#[derive(Debug, PartialEq)]
struct Args {
    base: String,
    head: String,
    runs: usize,
    command: Vec<String>,
}

fn parse_args(args: &[String]) -> Result<Args, String> {
    let split = args.iter().position(|a| a == "--").ok_or(USAGE)?;
    let (ours, command) = (&args[..split], &args[split + 1..]);
    if command.is_empty() {
        return Err(USAGE.to_owned());
    }
    let mut parsed = Args { base: String::new(), head: "HEAD".to_owned(), runs: 2, command: command.to_vec() };
    let mut ours = ours.iter();
    while let Some(arg) = ours.next() {
        match arg.as_str() {
            "--head" => parsed.head = ours.next().ok_or(USAGE)?.clone(),
            "--runs" => parsed.runs = ours.next().and_then(|n| n.parse().ok()).filter(|n| *n > 0).ok_or(USAGE)?,
            base if parsed.base.is_empty() && !base.starts_with("--") => parsed.base = base.to_owned(),
            _ => return Err(USAGE.to_owned()),
        }
    }
    if parsed.base.is_empty() {
        return Err(USAGE.to_owned());
    }
    Ok(parsed)
}

/// The readings in what a run printed, in the order first seen: Criterion's median by its benchmark's name, and every
/// `key=value` token, as printed
fn readings(output: &str) -> Vec<(String, String)> {
    let time = regex::Regex::new(r"time:\s+\[\S+ \S+ (\S+ \S+) \S+ \S+\]").expect("a valid pattern");
    let pair = regex::Regex::new(r"(?:^|\s)([A-Za-z_][A-Za-z_0-9]*)=(\S+)").expect("a valid pattern");
    let mut found = Vec::new();
    let mut name = None;
    for line in output.lines() {
        // A name stands at the line's start: with its time, or alone on the line before a long one's
        let first = line.split_whitespace().next().filter(|_| !line.starts_with(char::is_whitespace));
        if let Some(first) = first.filter(|first| first.contains('/') || time.is_match(line)) {
            name = Some(first.to_owned());
        }
        if let (Some(median), true) = (time.captures(line).and_then(|c| c.get(1)), name.is_some()) {
            found.push((name.take().expect("checked just above"), median.as_str().to_owned()));
        }
        for one in pair.captures_iter(line) {
            found.push((one[1].to_owned(), one[2].to_owned()));
        }
    }
    found
}

/// One run of one side: the load as it ended, if the machine says, and what it printed
struct Run {
    load: Option<f64>,
    readings: Vec<(String, String)>,
}

/// What a side read of `name`, a run to a cell: `-` for a run that didn't print it
fn cells(runs: &[Run], name: &str) -> String {
    let cell = |run: &Run| {
        let read: Vec<&str> =
            run.readings.iter().filter(|(held, _)| held == name).map(|(_, value)| value.as_str()).collect();
        if read.is_empty() { "-".to_owned() } else { read.join(" / ") }
    };
    runs.iter().map(cell).collect::<Vec<_>>().join(", ")
}

/// Each metric's readings on each side, in run order, under the load each run ended at, those past `cores` marked
fn table(base: &[Run], head: &[Run], cores: usize) -> String {
    let mut names: Vec<&str> = Vec::new();
    for run in base.iter().chain(head) {
        for (name, _) in &run.readings {
            if !names.contains(&name.as_str()) {
                names.push(name);
            }
        }
    }
    let load = |runs: &[Run]| {
        let each: Vec<String> = runs
            .iter()
            .map(|run| match run.load {
                Some(load) => format!("{load:.1}{}", if load > cores as f64 { " (past the cores)" } else { "" }),
                None => "unknown".to_owned(),
            })
            .collect();
        each.join(", ")
    };
    let mut out =
        format!("| | base | head |\n|---|---:|---:|\n| load as each run ended | {} | {} |\n", load(base), load(head));
    for name in names {
        out.push_str(&format!("| {name} | {} | {} |\n", cells(base, name), cells(head, name)));
    }
    out
}

fn git(cwd: &Path, args: &[&str]) -> Result<(), String> {
    let status = Command::new("git").args(args).current_dir(cwd).status().map_err(|e| format!("git: {e}"))?;
    if status.success() { Ok(()) } else { Err(format!("git {} failed", args.join(" "))) }
}

/// The commit `rev` names from `root`, the same for every run: `HEAD` or `HEAD~1` asked inside a side's worktree would be
/// that worktree's own
fn resolve(root: &Path, rev: &str) -> Result<String, String> {
    let output = Command::new("git")
        .args(["rev-parse", "--verify", &format!("{rev}^{{commit}}")])
        .current_dir(root)
        .output()
        .map_err(|e| format!("git: {e}"))?;
    if !output.status.success() {
        return Err(format!("no revision {rev}: {}", String::from_utf8_lossy(&output.stderr).trim()));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// The side's worktree at `commit`, made the first time, and nothing an earlier run left in it but what git ignores
fn tree(root: &Path, side: &str, commit: &str) -> Result<std::path::PathBuf, String> {
    let path = root.join("target/compare").join(side);
    if path.join(".git").exists() {
        git(&path, &["checkout", "--quiet", "--force", "--detach", commit])?;
        git(&path, &["clean", "--quiet", "--force", "-d"])?;
    } else {
        let at = path.to_str().ok_or("a path that isn't text")?;
        git(root, &["worktree", "add", "--quiet", "--detach", "--force", at, commit])?;
    }
    Ok(path)
}

/// Which side runs first in a run: each in turn, so a load that falls or rises through them favours neither
fn base_first(run: usize) -> bool {
    run.is_multiple_of(2)
}

/// The one-minute load average, from `uptime`, in the C locale so a decimal point is one; none if the machine won't say
fn load() -> Option<f64> {
    let output = Command::new("uptime").env("LC_ALL", "C").output().ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    let found = regex::Regex::new(r"load averages?:\s+([0-9.]+)").expect("a valid pattern");
    found.captures(&text).and_then(|c| c[1].parse().ok())
}

fn run(cwd: &Path, command: &[String]) -> Result<Run, String> {
    // Its build and its progress show as they happen. A target or a toolchain from the caller would make one of the sides'
    // builds and leave the revision's own `rust-toolchain.toml` unread
    let output = Command::new(&command[0])
        .args(&command[1..])
        .current_dir(cwd)
        .env_remove("CARGO_TARGET_DIR")
        .env_remove("RUSTUP_TOOLCHAIN")
        .stderr(std::process::Stdio::inherit())
        .output()
        .map_err(|e| format!("{}: {e}", command[0]))?;
    if !output.status.success() {
        return Err(format!("{} failed in {}", command.join(" "), cwd.display()));
    }
    Ok(Run { load: load(), readings: readings(&String::from_utf8_lossy(&output.stdout)) })
}

pub fn compare(root: &Path, args: &[String]) -> Result<(), String> {
    let args = parse_args(args)?;
    let (base_at, head_at) = (resolve(root, &args.base)?, resolve(root, &args.head)?);
    let (base_tree, head_tree) = (tree(root, "base", &base_at)?, tree(root, "head", &head_at)?);
    let (mut base, mut head) = (Vec::new(), Vec::new());
    let mut failed = None;
    'runs: for at in 0..args.runs {
        eprintln!("run {} of {}", at + 1, args.runs);
        let sides = if base_first(at) { [true, false] } else { [false, true] };
        for is_base in sides {
            let (tree, into) = if is_base { (&base_tree, &mut base) } else { (&head_tree, &mut head) };
            match run(tree, &args.command) {
                Ok(run) => into.push(run),
                Err(e) => {
                    failed = Some(e);
                    break 'runs;
                }
            }
        }
    }
    // What was read before a failure still counts for something
    let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
    println!(
        "{} ({}) against {} ({})\n\n{}",
        args.head,
        &head_at[..head_at.len().min(12)],
        args.base,
        &base_at[..base_at.len().min(12)],
        table(&base, &head, cores)
    );
    if let Some(e) = failed {
        return Err(e);
    }
    for (side, runs) in [("base", &base), ("head", &head)] {
        if runs.iter().all(|run| run.readings.is_empty()) {
            return Err(format!("{} printed no reading on {side}", args.command.join(" ")));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(args: &[&str]) -> Vec<String> {
        args.iter().map(|a| (*a).to_owned()).collect()
    }

    #[test]
    fn its_arguments_take_defaults_and_refuse_what_they_cant_read() {
        let parsed = parse_args(&strings(&["main", "--", "cargo", "bench"])).unwrap();
        assert_eq!(
            parsed,
            Args { base: "main".into(), head: "HEAD".into(), runs: 2, command: strings(&["cargo", "bench"]) }
        );
        let parsed = parse_args(&strings(&["main", "--head", "x", "--runs", "3", "--", "a"])).unwrap();
        assert_eq!((parsed.head.as_str(), parsed.runs), ("x", 3));
        for bad in
            [&["main"][..], &["--", "a"], &["main", "--"], &["main", "--runs", "0", "--", "a"], &["a", "b", "--", "c"]]
        {
            assert!(parse_args(&strings(bad)).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn it_reads_criterions_medians_under_their_names_and_key_value_tokens() {
        let printed = "\
graph/ranked/10000      time:   [7.6150 µs 7.6394 µs 7.6746 µs]
graph/rankIndexChange/1e-6/100000
                        time:   [366.91 ms 368.63 ms 370.61 ms]
                        change:
                        time:   [-5.2202% -3.6831% -2.0414%] (p = 0.00 < 0.05)
writes=3281 p50=75.708µs max=158.2ms
";
        assert_eq!(
            readings(printed),
            [
                ("graph/ranked/10000", "7.6394 µs"),
                ("graph/rankIndexChange/1e-6/100000", "368.63 ms"),
                ("writes", "3281"),
                ("p50", "75.708µs"),
                ("max", "158.2ms"),
            ]
            .map(|(a, b)| (a.to_owned(), b.to_owned()))
        );
    }

    #[test]
    fn its_table_keeps_every_run_and_marks_a_load_past_the_cores() {
        let run = |load, value: &str| Run { load: Some(load), readings: vec![("p50".to_owned(), value.to_owned())] };
        let written = table(&[run(3.0, "1ms"), run(12.0, "2ms")], &[run(4.0, "3ms")], 10);
        assert!(written.contains("| 3.0, 12.0 (past the cores) | 4.0 |"), "{written}");
        assert!(written.contains("| p50 | 1ms, 2ms | 3ms |"), "{written}");
    }

    #[test]
    fn its_table_shows_a_run_that_didnt_print_a_reading_and_one_that_printed_it_twice() {
        let run = |readings: &[(&str, &str)]| Run {
            load: None,
            readings: readings.iter().map(|(a, b)| ((*a).to_owned(), (*b).to_owned())).collect(),
        };
        let base = [run(&[("x", "1")]), run(&[("y", "2")]), run(&[("x", "3"), ("x", "4")])];
        let written = table(&base, &[], 10);
        assert!(written.contains("| x | 1, -, 3 / 4 |  |"), "{written}");
        assert!(written.contains("| y | -, 2, - |  |"), "{written}");
        assert!(written.contains("| load as each run ended | unknown, unknown, unknown |  |"), "{written}");
    }

    #[test]
    fn each_side_runs_first_in_turn() {
        assert_eq!((0..4).map(base_first).collect::<Vec<_>>(), [true, false, true, false]);
    }

    fn git_in(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false"])
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(status.status.success(), "{args:?}: {}", String::from_utf8_lossy(&status.stderr));
    }

    #[test]
    fn head_is_the_callers_on_every_run_and_a_side_keeps_nothing_an_earlier_run_left() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        git_in(root, &["init", "--quiet"]);
        std::fs::write(root.join("a"), "1").unwrap();
        git_in(root, &["add", "a"]);
        git_in(root, &["commit", "--quiet", "-m", "one"]);
        let first = resolve(root, "HEAD").unwrap();
        let side = tree(root, "head", &first).unwrap();
        assert_eq!(resolve(&side, "HEAD").unwrap(), first);
        std::fs::write(side.join("a"), "edited").unwrap();
        std::fs::write(side.join("left"), "behind").unwrap();
        std::fs::write(root.join("a"), "2").unwrap();
        git_in(root, &["commit", "--quiet", "-am", "two"]);
        // Asked from the checkout, not from inside the side, whose own HEAD is still the first
        let second = resolve(root, "HEAD").unwrap();
        assert_ne!(first, second);
        let side = tree(root, "head", &second).unwrap();
        assert_eq!(resolve(&side, "HEAD").unwrap(), second);
        assert_eq!(std::fs::read_to_string(side.join("a")).unwrap(), "2");
        assert!(!side.join("left").exists());
        assert!(resolve(root, "no-such-rev").is_err());
    }
}
