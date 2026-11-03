//! `cargo xtask compare`: one command on two revisions, in turn, each in a worktree of its own kept under `target/compare/`
//! so its builds stay warm, and every reading beside the machine's load when it was taken. It reports and decides nothing:
//! alternating is what keeps a load spike from reading as a change, and whether a run taken under one counts is the
//! reader's call.

use std::collections::BTreeMap;
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
        // A long name stands on the line before its time, and a change's own `time:` line follows with no name
        let first = line.split_whitespace().next();
        if let Some(first) = first.filter(|_| !line.starts_with(char::is_whitespace)) {
            if first.contains('/') {
                name = Some(first.to_owned());
            }
        }
        if let Some(median) = time.captures(line).and_then(|c| c.get(1)) {
            if let Some(name) = name.take() {
                found.push((name, median.as_str().to_owned()));
            }
        }
        for one in pair.captures_iter(line) {
            found.push((one[1].to_owned(), one[2].to_owned()));
        }
    }
    found
}

/// One run of one side
struct Run {
    load: f64,
    readings: Vec<(String, String)>,
}

/// Each metric's readings on each side, in run order, under the load each run began at, those past `cores` marked
fn table(base: &[Run], head: &[Run], cores: usize) -> String {
    let mut names = Vec::new();
    let mut held: BTreeMap<&str, (Vec<&str>, Vec<&str>)> = BTreeMap::new();
    for (side, runs) in [(0, base), (1, head)] {
        for run in runs {
            for (name, value) in &run.readings {
                if !held.contains_key(name.as_str()) {
                    names.push(name.as_str());
                }
                let entry = held.entry(name).or_default();
                if side == 0 { entry.0.push(value) } else { entry.1.push(value) }
            }
        }
    }
    let load = |runs: &[Run]| {
        let each: Vec<String> = runs
            .iter()
            .map(|run| format!("{:.1}{}", run.load, if run.load > cores as f64 { " (past the cores)" } else { "" }))
            .collect();
        each.join(", ")
    };
    let mut out =
        format!("| | base | head |\n|---|---:|---:|\n| load at each run's start | {} | {} |\n", load(base), load(head));
    for name in names {
        let (b, h) = &held[name];
        out.push_str(&format!("| {name} | {} | {} |\n", b.join(", "), h.join(", ")));
    }
    out
}

fn git(cwd: &Path, args: &[&str]) -> Result<(), String> {
    let status = Command::new("git").args(args).current_dir(cwd).status().map_err(|e| format!("git: {e}"))?;
    if status.success() { Ok(()) } else { Err(format!("git {} failed", args.join(" "))) }
}

/// The side's worktree at `rev`, made the first time
fn tree(root: &Path, side: &str, rev: &str) -> Result<std::path::PathBuf, String> {
    let path = root.join("target/compare").join(side);
    if path.join(".git").exists() {
        git(&path, &["checkout", "--quiet", "--detach", rev])?;
    } else {
        let at = path.to_str().ok_or("a path that isn't text")?;
        git(root, &["worktree", "add", "--quiet", "--detach", "--force", at, rev])?;
    }
    Ok(path)
}

/// The one-minute load average, from `uptime`, which Linux and macOS both have
fn load() -> Result<f64, String> {
    let output = Command::new("uptime").output().map_err(|e| format!("uptime: {e}"))?;
    let text = String::from_utf8_lossy(&output.stdout);
    let found = regex::Regex::new(r"load averages?:\s+([0-9.]+)").expect("a valid pattern");
    found.captures(&text).and_then(|c| c[1].parse().ok()).ok_or_else(|| format!("no load average in {text:?}"))
}

fn run(cwd: &Path, command: &[String]) -> Result<Run, String> {
    let load = load()?;
    let output = Command::new(&command[0])
        .args(&command[1..])
        .current_dir(cwd)
        .output()
        .map_err(|e| format!("{}: {e}", command[0]))?;
    if !output.status.success() {
        return Err(format!(
            "{} failed in {}:\n{}",
            command.join(" "),
            cwd.display(),
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    Ok(Run { load, readings: readings(&String::from_utf8_lossy(&output.stdout)) })
}

pub fn compare(root: &Path, args: &[String]) -> Result<(), String> {
    let args = parse_args(args)?;
    let (base_tree, head_tree) = (tree(root, "base", &args.base)?, tree(root, "head", &args.head)?);
    let (mut base, mut head) = (Vec::new(), Vec::new());
    for at in 1..=args.runs {
        eprintln!("run {at} of {}: base, then head", args.runs);
        base.push(run(&base_tree, &args.command)?);
        head.push(run(&head_tree, &args.command)?);
    }
    if base.iter().chain(&head).all(|run| run.readings.is_empty()) {
        return Err(format!("{} printed no reading on either side", args.command.join(" ")));
    }
    let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
    println!("{} against {}\n\n{}", args.head, args.base, table(&base, &head, cores));
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
        let run = |load, value: &str| Run { load, readings: vec![("p50".to_owned(), value.to_owned())] };
        let written = table(&[run(3.0, "1ms"), run(12.0, "2ms")], &[run(4.0, "3ms")], 10);
        assert!(written.contains("| 3.0, 12.0 (past the cores) | 4.0 |"), "{written}");
        assert!(written.contains("| p50 | 1ms, 2ms | 3ms |"), "{written}");
    }
}
