use std::process::Command;

use crate::metrics::{HostInfo, RekallInfo, RunEnvironment};

fn read_file_trimmed(path: &str) -> String {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .trim()
        .to_string()
}

fn command_output(cmd: &str, args: &[&str]) -> Option<String> {
    Command::new(cmd)
        .args(args)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
}

fn collect_hostname() -> String {
    read_file_trimmed("/etc/hostname")
}

fn collect_os() -> String {
    if let Ok(content) = std::fs::read_to_string("/etc/os-release") {
        for line in content.lines() {
            if let Some(val) = line.strip_prefix("PRETTY_NAME=") {
                return val.trim_matches('"').to_string();
            }
        }
    }
    String::from("unknown")
}

fn collect_kernel() -> String {
    read_file_trimmed("/proc/version")
        .split_whitespace()
        .nth(2)
        .unwrap_or("unknown")
        .to_string()
}

fn collect_cpu_model() -> String {
    if let Ok(content) = std::fs::read_to_string("/proc/cpuinfo") {
        for line in content.lines() {
            if let Some(val) = line.strip_prefix("model name") {
                return val.trim_start_matches([':', '\t', ' ']).to_string();
            }
        }
    }
    String::from("unknown")
}

fn collect_cpu_count() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
}

fn collect_memory_gb() -> f64 {
    if let Ok(content) = std::fs::read_to_string("/proc/meminfo") {
        for line in content.lines() {
            if let Some(val) = line.strip_prefix("MemTotal:") {
                let kb: f64 = val
                    .split_whitespace()
                    .next()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0.0);
                return (kb / 1_048_576.0 * 10.0).round() / 10.0;
            }
        }
    }
    0.0
}

fn collect_ip_address() -> Option<String> {
    command_output("ip", &["-4", "route", "get", "1.1.1.1"]).and_then(|out| {
        out.split_whitespace()
            .skip_while(|&w| w != "src")
            .nth(1)
            .map(String::from)
    })
}

/// Best-effort cloud instance type. An explicit `REKALL_INSTANCE_TYPE`
/// wins (works on any provider / bare metal); otherwise we query the AWS
/// EC2 Instance Metadata Service (IMDSv2, falling back to v1) with short
/// timeouts so a local run never stalls on the unroutable 169.254 address.
/// Returns `None` when not on EC2 or `curl` is unavailable.
fn collect_instance_type() -> Option<String> {
    if let Ok(v) = std::env::var("REKALL_INSTANCE_TYPE") {
        let v = v.trim();
        if !v.is_empty() {
            return Some(v.to_string());
        }
    }

    const URL: &str = "http://169.254.169.254/latest/meta-data/instance-type";

    // IMDSv2: fetch a short-lived token, then the metadata with it. If the
    // token request fails (IMDSv1-only host), try the bare GET.
    let token = command_output(
        "curl",
        &[
            "-s",
            "--max-time",
            "1",
            "-X",
            "PUT",
            "http://169.254.169.254/latest/api/token",
            "-H",
            "X-aws-ec2-metadata-token-ttl-seconds: 60",
        ],
    );

    let raw = match token {
        Some(t) => {
            let header = format!("X-aws-ec2-metadata-token: {t}");
            command_output("curl", &["-s", "--max-time", "1", URL, "-H", &header])
        }
        None => command_output("curl", &["-s", "--max-time", "1", URL]),
    }?;

    // Guard against an error page / redirect body sneaking through: a real
    // instance type is a single token like "r8i.2xlarge".
    let t = raw.trim();
    if t.is_empty() || t.contains(char::is_whitespace) || t.contains('<') {
        None
    } else {
        Some(t.to_string())
    }
}

pub fn collect_host_info() -> HostInfo {
    HostInfo {
        hostname: collect_hostname(),
        os: collect_os(),
        kernel: collect_kernel(),
        cpu_model: collect_cpu_model(),
        cpu_count: collect_cpu_count(),
        memory_gb: collect_memory_gb(),
        ip_address: collect_ip_address(),
        instance_type: collect_instance_type(),
    }
}

pub fn collect_rekall_info() -> RekallInfo {
    // Prefer the commit/branch embedded at build time (see build.rs) — that's
    // available regardless of the run's cwd. Fall back to a runtime
    // `git rev-parse` for the dev case of building and running inside a clone.
    let commit = non_empty(env!("REKALL_GIT_COMMIT"))
        .or_else(|| command_output("git", &["rev-parse", "--short", "HEAD"]));
    let branch = non_empty(env!("REKALL_GIT_BRANCH"))
        .or_else(|| command_output("git", &["rev-parse", "--abbrev-ref", "HEAD"]));
    RekallInfo {
        version: env!("CARGO_PKG_VERSION").to_string(),
        git_commit: commit,
        git_branch: branch,
        note: "rekall git info is captured at build time (or the local repo at \
               benchmark time); extension versions in pg_runtime.extensions are \
               authoritative (queried from the running PG instance)"
            .to_string(),
    }
}

/// Human-readable version for `rekall --version`: the crate version plus the
/// build-time git commit and branch when available, e.g.
/// `0.1.0 (a91106a / main)`. Falls back to just the version when no git info
/// was embedded.
pub fn version_string() -> String {
    let info = collect_rekall_info();
    match (info.git_commit, info.git_branch) {
        (Some(c), Some(b)) => format!("{} ({c} / {b})", info.version),
        (Some(c), None) => format!("{} ({c})", info.version),
        _ => info.version,
    }
}

fn non_empty(s: &str) -> Option<String> {
    let s = s.trim();
    (!s.is_empty()).then(|| s.to_string())
}

pub fn collect(comment: Option<String>) -> RunEnvironment {
    RunEnvironment {
        host: collect_host_info(),
        rekall: collect_rekall_info(),
        timestamp: chrono::Local::now()
            .format("%Y-%m-%dT%H:%M:%S%z")
            .to_string(),
        comment: comment
            .map(|c| c.trim().to_string())
            .filter(|c| !c.is_empty()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_empty_trims_and_nullifies_blank() {
        assert_eq!(non_empty("  abc "), Some("abc".to_string()));
        assert_eq!(non_empty("   "), None);
        assert_eq!(non_empty(""), None);
    }

    #[test]
    fn rekall_info_reports_commit_when_built_in_repo() {
        // build.rs embeds the commit; this crate is built inside the repo, so
        // the commit must be present (build-time value, cwd-independent).
        let info = collect_rekall_info();
        assert!(
            info.git_commit.is_some(),
            "expected an embedded rekall git commit"
        );
    }
}
