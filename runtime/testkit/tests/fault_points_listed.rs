//! `jamjet_state::faults::POINTS` lists exactly the fault points the code
//! declares: a point missing from the list is never failed by the faults
//! cases, and a listed point the code no longer has is a case that tests
//! nothing.

use jamjet_state::faults::POINTS;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// The source trees that may hold `fault_point!` calls, under `runtime/`.
const TREES: &[&str] = &["state/src", "scheduler/src", "audit/src", "api/src"];

const CALL: &str = "fault_point!(";

#[test]
fn every_point_in_the_code_is_listed() {
    let runtime = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("the kit sits in runtime/");
    // Each point, with where the code declares it.
    let mut in_code: BTreeMap<(String, u32), Vec<String>> = BTreeMap::new();
    for tree in TREES {
        let dir = runtime.join(tree);
        assert!(dir.is_dir(), "{} is not a directory", dir.display());
        for file in rust_files(&dir) {
            for (line, point) in points_in(&file) {
                let at = format!("{}:{line}", file.display());
                in_code.entry(point).or_default().push(at);
            }
        }
    }

    // One place per point within a file. A label may appear in two files,
    // as a named seam shared by two backends would.
    for ((label, step), places) in &in_code {
        let mut files: Vec<&str> = places
            .iter()
            .map(|p| p.rsplit_once(':').expect("file:line").0)
            .collect();
        files.sort_unstable();
        let before = files.len();
        files.dedup();
        assert_eq!(
            files.len(),
            before,
            "{label}#{step} is declared twice in one file: {places:?}"
        );
    }

    let listed: BTreeSet<(String, u32)> = POINTS.iter().map(|(l, s)| (l.to_string(), *s)).collect();
    assert_eq!(
        listed.len(),
        POINTS.len(),
        "faults::POINTS lists a point twice"
    );
    let code: BTreeSet<(String, u32)> = in_code.keys().cloned().collect();
    let unlisted: Vec<String> = code
        .difference(&listed)
        .map(|(l, s)| format!("{l}#{s} at {:?}", in_code[&(l.clone(), *s)]))
        .collect();
    let gone: Vec<String> = listed
        .difference(&code)
        .map(|(l, s)| format!("{l}#{s}"))
        .collect();
    assert!(
        unlisted.is_empty() && gone.is_empty(),
        "in the code but not in faults::POINTS: {unlisted:#?}\n\
         in faults::POINTS but not in the code: {gone:#?}"
    );

    // Each label's steps are 1, 2, ... with none left out.
    let mut steps: BTreeMap<&str, Vec<u32>> = BTreeMap::new();
    for (label, step) in POINTS {
        steps.entry(label).or_default().push(*step);
    }
    for (label, mut seen) in steps {
        seen.sort_unstable();
        let expected: Vec<u32> = (1..=seen.len() as u32).collect();
        assert_eq!(seen, expected, "the steps of {label} are not numbered 1..n");
    }
}

/// Each commit in a function that has fault points has one right before it,
/// at the commit's own indentation, so not inside a loop or a branch the
/// commit is outside of. An error armed there stops the call after every
/// statement before the commit has run, so it finds a write that a commit
/// split off anywhere earlier. A point inside a loop cannot: an error armed
/// there stops the first pass, and no armed point is reached after the
/// passes that follow.
#[test]
fn every_commit_has_a_point_right_before_it() {
    let runtime = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("the kit sits in runtime/");
    let mut missing = Vec::new();
    for tree in TREES {
        for file in rust_files(&runtime.join(tree)) {
            missing.extend(commits_without_a_point(&file));
        }
    }
    assert!(
        missing.is_empty(),
        "a commit with no fault_point! on the line before it, at its own indentation: \
         {missing:#?}"
    );
}

/// The commits in `file`, in a function that has a `fault_point!` call before
/// them, whose line before (blank and comment lines aside) is not a
/// `fault_point!` call indented as the commit is.
fn commits_without_a_point(file: &Path) -> Vec<String> {
    let text =
        std::fs::read_to_string(file).unwrap_or_else(|e| panic!("read {}: {e}", file.display()));
    let lines: Vec<&str> = text.lines().collect();
    let indent = |line: &str| line.len() - line.trim_start().len();
    let is_code = |line: &str| {
        let code = line.trim_start();
        !code.is_empty() && !code.starts_with("//")
    };
    let mut missing = Vec::new();
    // Whether the function the scan is in has a point so far.
    let mut pointed = false;
    for (n, line) in lines.iter().enumerate() {
        if !is_code(line) {
            continue;
        }
        let code = line.trim_start();
        if starts_a_function(code) {
            pointed = false;
        }
        if code.contains(CALL) {
            pointed = true;
        }
        if pointed && code.contains(".commit()") {
            let before = lines[..n].iter().rev().find(|l| is_code(l));
            let ok = before
                .is_some_and(|b| b.trim_start().starts_with(CALL) && indent(b) == indent(line));
            if !ok {
                missing.push(format!("{}:{}", file.display(), n + 1));
            }
        }
    }
    missing
}

/// Whether a trimmed line declares a function: `fn`, `async fn`, either
/// after a `pub` or `pub(..)`.
fn starts_a_function(code: &str) -> bool {
    let code = match code.strip_prefix("pub") {
        Some(rest) if rest.starts_with([' ', '(']) => rest.split_once(' ').map_or(rest, |(_, r)| r),
        _ => code,
    };
    code.starts_with("fn ") || code.starts_with("async fn ")
}

/// Every `.rs` file under `dir`, in a stable order.
fn rust_files(dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("read {}: {e}", dir.display()))
        .map(|entry| entry.expect("a directory entry").path())
        .collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            found.extend(rust_files(&path));
        } else if path.extension().is_some_and(|e| e == "rs") {
            found.push(path);
        }
    }
    found
}

/// The `fault_point!("label", step)` calls in `file`, with their line
/// numbers. Comment lines are skipped, so docs can name the macro. A call
/// written any other way panics, so it cannot hide from the list.
fn points_in(file: &Path) -> Vec<(usize, (String, u32))> {
    let text =
        std::fs::read_to_string(file).unwrap_or_else(|e| panic!("read {}: {e}", file.display()));
    let mut points = Vec::new();
    for (n, line) in text.lines().enumerate() {
        if line.trim_start().starts_with("//") {
            continue;
        }
        let mut rest = line;
        while let Some(at) = rest.find(CALL) {
            rest = &rest[at + CALL.len()..];
            let point = parse_call(rest).unwrap_or_else(|| {
                panic!(
                    "{}:{}: a fault_point! call must read fault_point!(\"label\", <step>) on one line: {line}",
                    file.display(),
                    n + 1
                )
            });
            points.push((n + 1, point));
        }
    }
    points
}

/// `"label", 3)` and what follows, as `("label", 3)`.
fn parse_call(args: &str) -> Option<(String, u32)> {
    let args = args.strip_prefix('"')?;
    let (label, rest) = args.split_once('"')?;
    let rest = rest.trim_start().strip_prefix(',')?.trim_start();
    let (step, _) = rest.split_once(')')?;
    let step: u32 = step.trim().parse().ok()?;
    let well_formed = !label.is_empty()
        && label
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '.');
    well_formed.then(|| (label.to_string(), step))
}
