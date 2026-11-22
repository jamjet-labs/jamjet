//! Records the commit the engine was built from, for `GET /health`.
//! See `build_commit.rs` for the order of sources.

#[path = "build_commit.rs"]
mod build_commit;

use std::path::PathBuf;
use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let dir = std::env::var("CARGO_MANIFEST_DIR").ok()?;
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn main() {
    println!("cargo:rerun-if-env-changed=JAMJET_BUILD_COMMIT");
    println!("cargo:rerun-if-env-changed=GIT_DIR");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=build_commit.rs");

    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap_or_default());
    let env = std::env::var("JAMJET_BUILD_COMMIT").ok();
    let vcs = std::fs::read_to_string(manifest.join(".cargo_vcs_info.json")).ok();

    // Rebuild when HEAD moves. `--git-path` works in a worktree, where `.git`
    // is a file.
    if let Some(head) = git(&["rev-parse", "--git-path", "HEAD"]) {
        println!(
            "cargo:rerun-if-changed={}",
            resolve(&manifest, &head).display()
        );
    }
    if let Some(r) = git(&["symbolic-ref", "-q", "HEAD"]) {
        if let Some(p) = git(&["rev-parse", "--git-path", &r]) {
            println!(
                "cargo:rerun-if-changed={}",
                resolve(&manifest, &p).display()
            );
        }
    }
    if let Some(p) = git(&["rev-parse", "--git-path", "packed-refs"]) {
        println!(
            "cargo:rerun-if-changed={}",
            resolve(&manifest, &p).display()
        );
    }

    match build_commit::resolve_commit(env.as_deref(), vcs.as_deref(), || {
        git(&["rev-parse", "HEAD"])
    }) {
        Ok((commit, source)) => {
            println!(
                "cargo:rustc-env=JAMJET_COMMIT={}",
                commit.unwrap_or_default()
            );
            println!("cargo:rustc-env=JAMJET_COMMIT_SOURCE={source}");
        }
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    }
}

/// `git --git-path` prints a path relative to the directory git ran in.
fn resolve(manifest: &std::path::Path, p: &str) -> PathBuf {
    let p = PathBuf::from(p);
    if p.is_absolute() {
        p
    } else {
        manifest.join(p)
    }
}
