// Copyright (c) 2026 Tiger Data, Inc.
// Licensed under the PostgreSQL License. See LICENSE for details.

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

    // Re-run when the commit changes so the embedded value doesn't go stale.
    // Watching just `.git/HEAD` isn't enough: on a branch, HEAD only changes
    // when you *switch* branches — a new commit on the same branch updates the
    // ref file HEAD points at (`.git/refs/heads/<branch>`) instead. So also
    // watch that ref, plus `packed-refs` (refs may be packed away) and the
    // reflog `logs/HEAD` (updated on every commit/checkout) as fallbacks.
    // Paths are relative to this package dir; the workspace `.git` is one
    // level up.
    for git_dir in ["../.git", ".git"] {
        let head = format!("{git_dir}/HEAD");
        if !std::path::Path::new(&head).exists() {
            continue;
        }
        println!("cargo:rerun-if-changed={head}");
        if let Ok(contents) = std::fs::read_to_string(&head) {
            if let Some(ref_path) = contents.strip_prefix("ref:").map(str::trim) {
                println!("cargo:rerun-if-changed={git_dir}/{ref_path}");
            }
        }
        println!("cargo:rerun-if-changed={git_dir}/packed-refs");
        println!("cargo:rerun-if-changed={git_dir}/logs/HEAD");
        break;
    }
}
