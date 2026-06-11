//! Embed rekall's own git commit/branch at build time. The runtime
//! `git rev-parse` in `environment.rs` only works when the process runs inside
//! a checkout — but `rekall infra run` launches benchmarks from `~`, where
//! there's no repo, so the commit came back empty. Capturing it here records
//! the commit the binary was *built* from, regardless of the run's cwd.

use std::process::Command;

fn git(args: &[&str]) -> String {
    Command::new("git")
        .args(args)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

fn main() {
    let commit = git(&["rev-parse", "--short", "HEAD"]);
    let branch = git(&["rev-parse", "--abbrev-ref", "HEAD"]);
    println!("cargo:rustc-env=REKALL_GIT_COMMIT={commit}");
    println!("cargo:rustc-env=REKALL_GIT_BRANCH={branch}");

    // Re-run when HEAD (or the branch ref it points at) moves, so the embedded
    // commit doesn't go stale across rebuilds. Paths are relative to this
    // package dir; the workspace `.git` is one level up.
    for p in ["../.git/HEAD", ".git/HEAD"] {
        if std::path::Path::new(p).exists() {
            println!("cargo:rerun-if-changed={p}");
        }
    }
}
