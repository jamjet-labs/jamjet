//! Two trait implementations that must not fall behind the trait.
//!
//! - The Postgres store overrides every `StateBackend` method the SQLite
//!   stores override. A method left to its trait default on Postgres would
//!   run the default's fallback, which for several methods writes in steps
//!   that two engines can interleave. One Postgres type serves unscoped and
//!   tenant-scoped, so it answers for both SQLite types: `SqliteBackend`
//!   and `TenantScopedSqliteBackend`. A Postgres method that still answers
//!   `not_yet` counts as not overridden, so while the store is being built
//!   this test lists what is left to write.
//! - `Intercept` delegates every trait method, so a test through it runs the
//!   wrapped backend's own code.
//!
//! The scan reads the source as rustfmt lays it out: an impl or trait block
//! ends at the first line that is exactly `}`, and its methods are the lines
//! that start with four spaces and `async fn`.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// Methods only the Postgres store overrides: the store's clock is the
/// database's there, and the process's on SQLite (the trait default).
const ONLY_ON_POSTGRES: &[&str] = &["store_now"];

/// What a Postgres method that is not written yet calls.
const STUB: &str = "not_yet(\"";

#[test]
fn postgres_overrides_what_sqlite_overrides() {
    let mut sqlite = BTreeSet::new();
    for (file, header) in [
        (
            "state/src/sqlite.rs",
            "impl StateBackend for SqliteBackend {",
        ),
        (
            "state/src/tenant_scoped.rs",
            "impl StateBackend for TenantScopedSqliteBackend {",
        ),
    ] {
        let found = methods(&block(&read(&runtime().join(file)), header));
        assert!(
            found.contains("commit_turn") && found.contains("append_event"),
            "the scan of {file} found {found:?}"
        );
        sqlite.extend(found);
    }

    let (file, postgres) = postgres_impl();
    let declared = methods(&postgres);
    let waiting = stubs(&postgres);
    let written: BTreeSet<String> = declared.difference(&waiting).cloned().collect();

    let missing: Vec<&String> = sqlite.difference(&written).collect();
    let extra: Vec<&String> = written
        .difference(&sqlite)
        .filter(|m| !ONLY_ON_POSTGRES.contains(&m.as_str()))
        .collect();
    assert!(
        missing.is_empty() && extra.is_empty(),
        "the SQLite stores override these and the Postgres store ({}) has no body for them \
         yet ({} of {}): {missing:#?}\n\
         the Postgres store overrides these and neither SQLite store does: {extra:#?}",
        file.display(),
        missing.len(),
        sqlite.len()
    );
}

#[test]
fn intercept_delegates_every_trait_method() {
    let trait_methods = methods(&block(
        &read(&runtime().join("state/src/backend.rs")),
        "pub trait StateBackend: Send + Sync {",
    ));
    assert!(
        trait_methods.contains("commit_turn") && trait_methods.contains("open_work_items"),
        "the scan of the trait found {trait_methods:?}"
    );
    let intercept = methods(&block(
        &read(&runtime().join("testkit/src/intercept.rs")),
        "impl StateBackend for Intercept {",
    ));
    let missing: Vec<&String> = trait_methods.difference(&intercept).collect();
    let unknown: Vec<&String> = intercept.difference(&trait_methods).collect();
    assert!(
        missing.is_empty() && unknown.is_empty(),
        "Intercept leaves these trait methods to their defaults: {missing:#?}\n\
         Intercept has these, the trait does not: {unknown:#?}"
    );
}

/// The scan on a small text: methods at the block's own indentation only,
/// stubs only where they name their own method, and the block's end.
#[test]
fn the_scan_reads_methods_and_stubs() {
    let text = "\
impl StateBackend for Toy {
    // ── A section ──
    async fn written(&self) -> BackendResult<()> {
        async fn nested() {}
        Ok(())
    }

    async fn waiting(
        &self,
    ) -> BackendResult<()> {
        not_yet(\"waiting\")
    }
}

async fn after_the_block() {}
";
    let toy = block(text, "impl StateBackend for Toy {");
    assert_eq!(methods(&toy), set(&["written", "waiting"]));
    assert_eq!(stubs(&toy), set(&["waiting"]));

    let mislabelled = text.replace("not_yet(\"waiting\")", "not_yet(\"written\")");
    let caught =
        std::panic::catch_unwind(|| stubs(&block(&mislabelled, "impl StateBackend for Toy {")));
    assert!(
        caught.is_err(),
        "a stub naming another method was let through"
    );
}

fn set(names: &[&str]) -> BTreeSet<String> {
    names.iter().map(|n| n.to_string()).collect()
}

fn runtime() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("the kit sits in runtime/")
        .to_path_buf()
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// The lines of the block that `header` opens, up to the first line that is
/// exactly `}`. `header` must start a line, once.
fn block(text: &str, header: &str) -> Vec<String> {
    let lines: Vec<&str> = text.lines().collect();
    let starts: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, l)| l.starts_with(header))
        .map(|(i, _)| i)
        .collect();
    assert_eq!(starts.len(), 1, "`{header}` starts {} lines", starts.len());
    let start = starts[0];
    let end = lines[start + 1..]
        .iter()
        .position(|l| *l == "}")
        .unwrap_or_else(|| panic!("`{header}` has no closing line"))
        + start
        + 1;
    lines[start + 1..end]
        .iter()
        .map(|l| l.to_string())
        .collect()
}

/// The `async fn` names at a block's own indentation.
fn methods(block: &[String]) -> BTreeSet<String> {
    block.iter().filter_map(|l| method_name(l)).collect()
}

fn method_name(line: &str) -> Option<String> {
    let rest = line.strip_prefix("    async fn ")?;
    let name: String = rest
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
        .collect();
    (!name.is_empty()).then_some(name)
}

/// The methods whose body calls `not_yet`. Each such call must name the
/// method it sits in, so a stub cannot hide a method it does not belong to.
fn stubs(block: &[String]) -> BTreeSet<String> {
    let mut found = BTreeSet::new();
    let mut current: Option<String> = None;
    for line in block {
        if let Some(name) = method_name(line) {
            current = Some(name);
        }
        let Some(at) = line.find(STUB) else { continue };
        let named: String = line[at + STUB.len()..]
            .chars()
            .take_while(|c| *c != '"')
            .collect();
        let inside = current
            .clone()
            .unwrap_or_else(|| panic!("`{}` is outside any method", line.trim()));
        assert_eq!(named, inside, "a not_yet stub names another method");
        found.insert(inside);
    }
    found
}

/// The file and lines of `impl StateBackend for PgBackend`, which sits in
/// one file under `state/src/postgres`.
fn postgres_impl() -> (PathBuf, Vec<String>) {
    let header = "impl StateBackend for PgBackend {";
    let dir = runtime().join("state/src/postgres");
    let mut found = Vec::new();
    let mut entries: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("read {}: {e}", dir.display()))
        .map(|e| e.expect("a directory entry").path())
        .filter(|p| p.extension().is_some_and(|x| x == "rs"))
        .collect();
    entries.sort();
    for path in entries {
        let text = read(&path);
        if text.lines().any(|l| l.starts_with(header)) {
            found.push((path, block(&text, header)));
        }
    }
    assert_eq!(
        found.len(),
        1,
        "`{header}` sits in {} files under {}",
        found.len(),
        dir.display()
    );
    found.pop().expect("one file")
}
