// Copyright (c) 2026 Tiger Data, Inc.
// Licensed under the PostgreSQL License. See LICENSE for details.

//! Crash / unexpected-disconnect diagnostics.
//!
//! When a benchmark operation (an index build or a search query) fails
//! because the PostgreSQL backend crashed, the server was shut down, or the
//! connection dropped, we want more than the bare client error — the cause
//! usually lives in the server log, a core dump, or the kernel OOM killer.
//!
//! [`report_if_crash`] classifies the error and, when it looks like a crash
//! or unexpected disconnect, writes a `crash-*.txt` report into the results
//! directory. Collection is best-effort and platform-aware: server logs,
//! systemd-coredump cores, the service journal, and kernel messages are only
//! reachable when PostgreSQL is on the same (Linux) host as rekall. For a
//! remote DSN we record the client-side error and note where to look.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::error::Error;

/// Classify a PostgreSQL error. Returns a human-readable reason when the
/// error indicates a crash, shutdown, or dropped connection (as opposed to
/// an ordinary query error like a syntax or type error), else `None`.
pub fn crash_reason(err: &tokio_postgres::Error) -> Option<String> {
    if let Some(db) = err.as_db_error() {
        let code = db.code().code();
        let reason = match code {
            "57P01" => "server shut down (administrator command / fast shutdown)",
            "57P02" => "another server process crashed (crash shutdown)",
            "57P03" => "server in recovery, not accepting connections",
            c if c.starts_with("08") => "connection exception",
            _ => return None, // ordinary SQL error — not a crash
        };
        return Some(format!("{reason} [SQLSTATE {code}]"));
    }
    // No SQLSTATE → the connection itself broke rather than the statement.
    if err.is_closed() {
        return Some(
            "connection closed unexpectedly — the server may have crashed or been shut down"
                .to_string(),
        );
    }
    Some(format!("connection/transport error: {err}"))
}

/// If `err` wraps a PostgreSQL crash/disconnect, gather a diagnostics report
/// into `results_dir` and return its path. Returns `None` for non-PG errors,
/// ordinary query errors, or if the report can't be written.
pub fn report_if_crash(
    results_dir: &str,
    dsn: &str,
    context: &str,
    err: &Error,
) -> Option<PathBuf> {
    let pg_err = match err {
        Error::Pg(e) => e,
        Error::PgConnect { source, .. } => source,
        _ => return None,
    };
    let reason = crash_reason(pg_err)?;
    write_report(
        results_dir,
        dsn,
        context,
        &reason,
        &describe_pg_error(pg_err),
    )
}

/// A useful description of a PG error. tokio_postgres's `Display` is terse
/// ("db error"); the real message/detail live in the `DbError`, and for a
/// transport failure the cause is in the error's `source` chain.
fn describe_pg_error(err: &tokio_postgres::Error) -> String {
    if let Some(db) = err.as_db_error() {
        let mut s = format!(
            "{}: {} [SQLSTATE {}]",
            db.severity(),
            db.message(),
            db.code().code()
        );
        if let Some(d) = db.detail() {
            let _ = write!(s, "\nDETAIL: {d}");
        }
        if let Some(h) = db.hint() {
            let _ = write!(s, "\nHINT: {h}");
        }
        if let Some(w) = db.where_() {
            let _ = write!(s, "\nWHERE: {w}");
        }
        s
    } else {
        let mut s = err.to_string();
        let mut src = std::error::Error::source(err);
        while let Some(e) = src {
            let _ = write!(s, "\ncaused by: {e}");
            src = e.source();
        }
        s
    }
}

fn write_report(
    results_dir: &str,
    dsn: &str,
    context: &str,
    reason: &str,
    err_text: &str,
) -> Option<PathBuf> {
    let local = is_local_dsn(dsn);
    let mut out = String::new();

    let _ = writeln!(out, "=== Rekall crash / disconnect diagnostics ===");
    let _ = writeln!(out, "context : {context}");
    let _ = writeln!(out, "reason  : {reason}");
    let _ = writeln!(out, "dsn     : {}", redact_dsn(dsn));
    let _ = writeln!(out, "locality: {}", if local { "local" } else { "remote" });
    let _ = writeln!(out, "\n--- client error ---\n{err_text}");

    if !local {
        let _ = writeln!(
            out,
            "\nPostgreSQL is remote — server log, core dumps, journal, and kernel\n\
             messages aren't reachable from here. Inspect them on the server:\n\
             \x20 journalctl -u 'postgresql@*-main' --since '-15min'\n\
             \x20 coredumpctl list\n\
             \x20 journalctl -k | grep -i 'out of memory'"
        );
    } else if cfg!(target_os = "linux") {
        collect_local_linux(&mut out);
    } else {
        let _ = writeln!(
            out,
            "\nLocal host is not Linux — systemd-coredump / journal / dmesg\n\
             collection is skipped. Check the PostgreSQL server log manually."
        );
        append_pg_log_tail(&mut out);
    }

    let path = next_report_path(results_dir, context);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    match std::fs::write(&path, out) {
        Ok(()) => Some(path),
        Err(e) => {
            tracing::error!(
                "failed to write crash diagnostics to {}: {e}",
                path.display()
            );
            None
        }
    }
}

/// Best-effort collection on a local Linux host: server log tail, plus the
/// systemd-coredump list, the postgresql unit journal, and recent kernel
/// (OOM) messages. Each command's output — or the reason it couldn't run —
/// is recorded so a missing tool or permission is visible in the report.
fn collect_local_linux(out: &mut String) {
    append_pg_log_tail(out);

    for (title, cmd, args) in [
        (
            "core dumps (coredumpctl list)",
            "coredumpctl",
            &["list", "--no-pager"][..],
        ),
        (
            "most recent core (coredumpctl info)",
            "coredumpctl",
            &["info"][..],
        ),
        (
            "postgresql service journal",
            "journalctl",
            &[
                "-u",
                "postgresql@*-main",
                "--since",
                "-15min",
                "-n",
                "200",
                "--no-pager",
            ][..],
        ),
        (
            "kernel messages (OOM killer etc.)",
            "journalctl",
            &["-k", "--since", "-15min", "-n", "120", "--no-pager"][..],
        ),
    ] {
        let _ = writeln!(out, "\n--- {title} ---");
        let _ = writeln!(out, "$ {cmd} {}", args.join(" "));
        match Command::new(cmd).args(args).output() {
            Ok(o) => {
                let stdout = String::from_utf8_lossy(&o.stdout);
                let stderr = String::from_utf8_lossy(&o.stderr);
                if !stdout.trim().is_empty() {
                    let _ = writeln!(out, "{}", stdout.trim_end());
                }
                if !stderr.trim().is_empty() {
                    let _ = writeln!(out, "[stderr] {}", stderr.trim_end());
                }
                if stdout.trim().is_empty() && stderr.trim().is_empty() {
                    let _ = writeln!(out, "(no output)");
                }
            }
            Err(e) => {
                let _ = writeln!(out, "(could not run {cmd}: {e})");
            }
        }
    }
}

/// Tail the standard Ubuntu/Debian PostgreSQL log files
/// (`/var/log/postgresql/*.log`). Best-effort — silent if the directory or
/// files aren't readable.
fn append_pg_log_tail(out: &mut String) {
    let dir = Path::new("/var/log/postgresql");
    let Ok(entries) = std::fs::read_dir(dir) else {
        let _ = writeln!(
            out,
            "\n--- PostgreSQL server log ---\n({} not readable)",
            dir.display()
        );
        return;
    };
    let mut logs: Vec<PathBuf> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "log"))
        .collect();
    logs.sort();
    if logs.is_empty() {
        let _ = writeln!(
            out,
            "\n--- PostgreSQL server log ---\n(no *.log in {})",
            dir.display()
        );
        return;
    }
    for log in logs {
        let _ = writeln!(
            out,
            "\n--- PostgreSQL server log: {} (tail) ---",
            log.display()
        );
        match std::fs::read_to_string(&log) {
            Ok(content) => {
                let lines: Vec<&str> = content.lines().collect();
                let start = lines.len().saturating_sub(80);
                let _ = writeln!(out, "{}", lines[start..].join("\n"));
            }
            Err(e) => {
                let _ = writeln!(out, "(could not read: {e})");
            }
        }
    }
}

/// A DSN is "local" if it uses a Unix socket, names a loopback host, or omits
/// the host (libpq then defaults to the local socket).
fn is_local_dsn(dsn: &str) -> bool {
    let host = dsn
        .split_whitespace()
        .find_map(|tok| tok.strip_prefix("host="));
    match host {
        None => true,                          // libpq default: local socket
        Some(h) if h.starts_with('/') => true, // Unix socket path
        Some(h) => matches!(h, "localhost" | "127.0.0.1" | "::1"),
    }
}

/// Drop a `password=...` token from a DSN before writing it to a file.
fn redact_dsn(dsn: &str) -> String {
    dsn.split_whitespace()
        .map(|tok| {
            if tok.starts_with("password=") {
                "password=***"
            } else {
                tok
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// `{results_dir}/crash-{slug}-{n}.txt`, picking the first free `n` so
/// concurrent/repeated crashes don't clobber each other. Avoids needing a
/// timestamp (the codebase forbids wall-clock in some paths and this keeps
/// the name deterministic in tests).
fn next_report_path(results_dir: &str, context: &str) -> PathBuf {
    let slug: String = context
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .collect();
    let slug = slug.trim_matches('-');
    for n in 0.. {
        let path = Path::new(results_dir).join(format!("crash-{slug}-{n}.txt"));
        if !path.exists() {
            return path;
        }
    }
    unreachable!()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_dsn_detection() {
        assert!(is_local_dsn("host=localhost port=5432"));
        assert!(is_local_dsn("host=127.0.0.1"));
        assert!(is_local_dsn("host=/var/run/postgresql port=5432"));
        assert!(is_local_dsn("port=5432 dbname=rekall")); // no host
        assert!(!is_local_dsn("host=10.0.0.5 port=5432"));
        assert!(!is_local_dsn("host=db.internal dbname=rekall"));
    }

    #[test]
    fn redact_strips_password() {
        assert_eq!(
            redact_dsn("host=db user=rekall password=secret dbname=rekall"),
            "host=db user=rekall password=*** dbname=rekall"
        );
    }

    #[test]
    fn report_path_is_slugged_and_unique() {
        let dir = std::env::temp_dir().join(format!("rekall-diag-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let d = dir.to_string_lossy();
        let p0 = next_report_path(&d, "build idx_rekall_cohere_5m/n4000");
        assert_eq!(
            p0.file_name().unwrap(),
            "crash-build-idx-rekall-cohere-5m-n4000-0.txt"
        );
        std::fs::write(&p0, "x").unwrap();
        let p1 = next_report_path(&d, "build idx_rekall_cohere_5m/n4000");
        assert!(p1
            .file_name()
            .unwrap()
            .to_string_lossy()
            .ends_with("-1.txt"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn report_if_crash_ignores_non_pg_errors() {
        let err = Error::Config("bad config".into());
        assert!(report_if_crash("/tmp", "host=localhost", "ctx", &err).is_none());
    }

    // Real end-to-end: terminate a backend mid-query and confirm a report is
    // written. Ignored by default — run with a DSN to a disposable PG:
    //   REKALL_TEST_DSN='host=/path port=5433 dbname=postgres' \
    //     cargo test -p rekall-lib diagnostics -- --ignored --nocapture
    #[tokio::test]
    #[ignore = "needs a local PG; set REKALL_TEST_DSN"]
    async fn end_to_end_report_on_backend_terminate() {
        let Ok(dsn) = std::env::var("REKALL_TEST_DSN") else {
            return;
        };
        use tokio_postgres::NoTls;

        let (client_a, conn_a) = tokio_postgres::connect(&dsn, NoTls).await.unwrap();
        let _ha = tokio::spawn(conn_a);
        let pid_a: i32 = client_a
            .query_one("SELECT pg_backend_pid()", &[])
            .await
            .unwrap()
            .get(0);

        let (client_b, conn_b) = tokio_postgres::connect(&dsn, NoTls).await.unwrap();
        let _hb = tokio::spawn(conn_b);

        let sleeper =
            tokio::spawn(async move { client_a.simple_query("SELECT pg_sleep(5)").await });
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        client_b
            .execute("SELECT pg_terminate_backend($1)", &[&pid_a])
            .await
            .unwrap();

        let err = sleeper.await.unwrap().expect_err("query should fail");
        assert!(crash_reason(&err).is_some(), "should classify as a crash");

        let dir = std::env::temp_dir().join(format!("rekall-e2e-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = report_if_crash(
            &dir.to_string_lossy(),
            &dsn,
            "e2e terminate",
            &Error::Pg(err),
        )
        .expect("a report should be written");
        let content = std::fs::read_to_string(&path).unwrap();
        println!("--- report at {} ---\n{content}", path.display());
        assert!(content.contains("client error"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
