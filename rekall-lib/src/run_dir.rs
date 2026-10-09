// Copyright (c) 2026 Tiger Data, Inc.
// Licensed under the PostgreSQL License. See LICENSE for details.

//! Results-directory housekeeping for clean (non-resuming) runs.
//!
//! A run normally writes into a fixed `results_dir` and *resumes*: completed
//! sweep points are skipped, ground truth is reused. To force a genuinely fresh
//! run on the same host without losing the prior results, `archive_results_dir`
//! moves the existing directory aside to a timestamped sibling
//! (`<dir>-<timestamp>`), leaving `<dir>` absent so the next run starts clean.

use std::path::{Path, PathBuf};

/// Move an existing, non-empty `results_dir` aside to `<dir>-<timestamp>` so a
/// new run starts fresh instead of resuming. The timestamp is taken from the
/// prior run's `run-info-<ts>.json` (so it reflects when that run started)
/// when available, else the current local time. A `_N` suffix is appended if
/// the target already exists.
///
/// Returns `Ok(None)` when there is nothing to archive (the directory is
/// missing or empty) and `Ok(Some(path))` with the archive location otherwise.
pub fn archive_results_dir(results_dir: &str) -> std::io::Result<Option<PathBuf>> {
    let dir = Path::new(results_dir);
    if !dir.exists() {
        return Ok(None);
    }
    // Empty directory → nothing worth preserving.
    if std::fs::read_dir(dir)?.next().is_none() {
        return Ok(None);
    }

    let ts = prior_run_timestamp(dir)
        .unwrap_or_else(|| chrono::Local::now().format("%Y%m%d-%H%M%S").to_string());

    let trimmed = results_dir.trim_end_matches('/');
    let base = format!("{trimmed}-{ts}");
    let mut target = PathBuf::from(&base);
    let mut n = 2;
    while target.exists() {
        target = PathBuf::from(format!("{base}_{n}"));
        n += 1;
    }

    std::fs::rename(dir, &target)?;
    Ok(Some(target))
}

/// Extract the timestamp token from the newest `run-info-<ts>.json` in `dir`.
/// `run-info` files are written early in a run, so the token reflects the
/// prior run's start time. Returns `None` if no such file is present.
fn prior_run_timestamp(dir: &Path) -> Option<String> {
    let mut stamps: Vec<String> = std::fs::read_dir(dir)
        .ok()?
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            name.strip_prefix("run-info-")
                .and_then(|s| s.strip_suffix(".json"))
                .map(str::to_string)
        })
        .collect();
    stamps.sort();
    stamps.pop()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_dir_is_noop() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("does-not-exist");
        let got = archive_results_dir(missing.to_str().unwrap()).unwrap();
        assert_eq!(got, None);
    }

    #[test]
    fn empty_dir_is_noop() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("results");
        std::fs::create_dir(&dir).unwrap();
        let got = archive_results_dir(dir.to_str().unwrap()).unwrap();
        assert_eq!(got, None);
        assert!(dir.exists(), "empty dir is left in place");
    }

    #[test]
    fn archives_using_run_info_timestamp() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("cohere-10m");
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(dir.join("run-info-20260610-095642.json"), "{}").unwrap();
        std::fs::write(dir.join("prism-n16000-20260610-100000.json.gz"), "x").unwrap();

        let archived = archive_results_dir(dir.to_str().unwrap()).unwrap().unwrap();

        assert_eq!(archived, tmp.path().join("cohere-10m-20260610-095642"));
        assert!(!dir.exists(), "original dir is moved away");
        assert!(archived
            .join("prism-n16000-20260610-100000.json.gz")
            .exists());
    }

    #[test]
    fn newest_run_info_timestamp_wins() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("ds");
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(dir.join("run-info-20260101-000000.json"), "{}").unwrap();
        std::fs::write(dir.join("run-info-20260610-095642.json"), "{}").unwrap();

        let archived = archive_results_dir(dir.to_str().unwrap()).unwrap().unwrap();
        assert_eq!(archived, tmp.path().join("ds-20260610-095642"));
    }

    #[test]
    fn target_collision_gets_suffix() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("ds");
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(dir.join("run-info-20260610-095642.json"), "{}").unwrap();
        // A prior archive with the same timestamp already exists.
        std::fs::create_dir(tmp.path().join("ds-20260610-095642")).unwrap();

        let archived = archive_results_dir(dir.to_str().unwrap()).unwrap().unwrap();
        assert_eq!(archived, tmp.path().join("ds-20260610-095642_2"));
    }
}
