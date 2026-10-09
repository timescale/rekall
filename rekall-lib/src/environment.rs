// Copyright (c) 2026 Tiger Data, Inc.
// Licensed under the PostgreSQL License. See LICENSE for details.

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

/// CPUs available to this process, honoring cgroup and affinity limits.
pub fn collect_cpu_count() -> usize {
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
        // Filled in later by the run, once PG has told us its
        // data_directory; the bare host collector has no DB connection.
        storage: None,
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

/// Observe the storage backing `data_dir` on the running machine.
///
/// Everything here is read from the machine: `/proc/mounts`, `/sys/block`,
/// the NVMe device serial, and (only for the provisioned ceiling) the EC2
/// API. Nothing is inferred from an instance config or an instance type,
/// because both have already proved capable of lying -- `disk_iops = 16000`
/// was silently dropped by the native launch path, and PGDATA can land on
/// the EBS root volume on a box provisioned for instance-store NVMe.
pub fn collect_storage_info(data_dir: Option<&str>) -> Option<crate::metrics::StorageInfo> {
    let mounts = std::fs::read_to_string("/proc/mounts").ok()?;
    let (mount_point, dev_path, filesystem) = data_dir
        .and_then(|d| mount_for_path(d, &mounts))
        .or_else(|| mount_for_path("/", &mounts))?;

    // Resolve the block device from the directory's st_dev rather than from
    // the device name in /proc/mounts. Ubuntu cloud images report the root
    // filesystem as `/dev/root`, which is not a real device node: it cannot
    // be canonicalized and leaves nothing /sys/block knows about, so name
    // parsing yielded a bogus device called "root" and every field below it
    // came back unknown. The major:minor pair is always right, and
    // /sys/dev/block maps it straight to the kernel's own name.
    let leaf = device_name_from_st_dev(data_dir.unwrap_or(&mount_point))
        .or_else(|| {
            let real = std::fs::canonicalize(&dev_path)
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|_| dev_path.clone());
            real.strip_prefix("/dev/").map(str::to_string)
        })
        .unwrap_or_else(|| dev_path.clone());
    let device = parent_block_device(&leaf);

    let sysfs = |f: &str| -> Option<String> {
        std::fs::read_to_string(format!("/sys/block/{device}/{f}"))
            .ok()
            .map(|s| s.trim().to_string())
    };

    // /sys/block reports size in 512-byte sectors regardless of the device's
    // own logical block size.
    let size_gb = sysfs("size")
        .and_then(|s| s.parse::<f64>().ok())
        .map(|sectors| sectors * 512.0 / 1e9);
    let rotational = sysfs("queue/rotational").map(|s| s == "1");
    // The scheduler file lists every option with the active one in [brackets].
    let scheduler = sysfs("queue/scheduler").map(|s| {
        s.split_whitespace()
            .find(|t| t.starts_with('['))
            .map(|t| t.trim_matches(['[', ']']).to_string())
            .unwrap_or(s)
    });
    let nr_requests = sysfs("queue/nr_requests").and_then(|s| s.parse().ok());
    let read_ahead_kb = sysfs("queue/read_ahead_kb").and_then(|s| s.parse().ok());

    let model = sysfs("device/model").unwrap_or_default();
    let ebs_volume_id = ebs_volume_id_for(&device);
    let media = classify_media(&model, ebs_volume_id.is_some(), rotational);

    let (ebs_volume_type, ebs_provisioned_iops, ebs_provisioned_throughput_mbs, ebs_source) =
        match ebs_volume_id.as_deref() {
            None => (None, None, None, "not an EBS volume".to_string()),
            Some(id) => match describe_ebs_volume(id) {
                Some((t, i, tp)) => (Some(t), i, tp, "ec2:DescribeVolumes".to_string()),
                // Never fall back to "probably the default": a guessed
                // ceiling is worse than a stated absence, since it would be
                // indistinguishable from a measured one on the results page.
                None => (
                    None,
                    None,
                    None,
                    "unavailable (ec2:DescribeVolumes failed or not permitted)".to_string(),
                ),
            },
        };

    Some(crate::metrics::StorageInfo {
        data_directory: data_dir.map(str::to_string),
        mount_point: Some(mount_point),
        filesystem: Some(filesystem),
        media,
        device,
        device_resolved_from_pgdata: data_dir.is_some(),
        size_gb,
        rotational,
        scheduler,
        nr_requests,
        read_ahead_kb,
        ebs_volume_id,
        ebs_volume_type,
        ebs_provisioned_iops,
        ebs_provisioned_throughput_mbs,
        ebs_source,
    })
}

/// Kernel block-device name for whatever `path` lives on, via its st_dev.
///
/// Immune to the names in /proc/mounts: `/dev/root`, LVM mapper symlinks and
/// bind mounts all resolve correctly because major:minor identifies the
/// device unambiguously and /sys/dev/block/<maj>:<min> is a symlink into the
/// device tree. Returns the leaf (e.g. "nvme0n1p1"); the caller walks up to
/// the parent device.
fn device_name_from_st_dev(path: &str) -> Option<String> {
    // linux::fs::MetadataExt, not unix::fs::MetadataExt: st_dev lives on the
    // Linux-specific trait.
    use std::os::linux::fs::MetadataExt;
    let dev = std::fs::metadata(path).ok()?.st_dev();
    // glibc dev_t packing: the major and minor are each split across two
    // ranges, so neither can be read with a single shift. nvme's major (259)
    // overflows the low 8-bit field, which is exactly the case a naive
    // `dev >> 8` gets wrong.
    let major = ((dev >> 8) & 0xfff) | ((dev >> 32) & !0xfffu64);
    let minor = (dev & 0xff) | ((dev >> 12) & !0xffu64);
    let link = std::fs::canonicalize(format!("/sys/dev/block/{major}:{minor}")).ok()?;
    Some(link.file_name()?.to_string_lossy().into_owned())
}

/// Longest-prefix mount lookup: (mount point, device path, fstype).
fn mount_for_path(path: &str, mounts: &str) -> Option<(String, String, String)> {
    let mut best: Option<(String, String, String)> = None;
    for line in mounts.lines() {
        let mut f = line.split_whitespace();
        let (dev, mnt, fstype) = (f.next()?, f.next()?, f.next()?);
        if !dev.starts_with("/dev/") {
            continue;
        }
        if path == mnt || path.starts_with(&format!("{}/", mnt.trim_end_matches('/'))) || mnt == "/"
        {
            let better = best.as_ref().is_none_or(|(m, _, _)| mnt.len() > m.len());
            if better {
                best = Some((mnt.to_string(), dev.to_string(), fstype.to_string()));
            }
        }
    }
    best
}

/// Strip a partition suffix to reach the parent device: `nvme0n1p1` ->
/// `nvme0n1`, `sda2` -> `sda`. Left alone when already a whole device.
fn parent_block_device(leaf: &str) -> String {
    if std::path::Path::new(&format!("/sys/block/{leaf}")).exists() {
        return leaf.to_string();
    }
    strip_partition_suffix(leaf).to_string()
}

/// The naming rules behind `parent_block_device`, with no look at the
/// machine. A device whose name ends in a digit (`nvme0n1`, `mmcblk0`)
/// numbers its partitions with a `p` (`nvme0n1p1`); one that ends in a
/// letter (`sda`, `vdb`) appends the digits directly (`sda2`). A bare nvme
/// namespace keeps its digits: they name the namespace, not a partition.
fn strip_partition_suffix(leaf: &str) -> &str {
    let all_digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    if let Some((head, tail)) = leaf.rsplit_once('p') {
        if all_digits(tail) && head.ends_with(|c: char| c.is_ascii_digit()) {
            return head;
        }
    }
    if let Some(rest) = leaf.strip_prefix("nvme") {
        if let Some((ctrl, ns)) = rest.split_once('n') {
            if all_digits(ctrl) && all_digits(ns) {
                return leaf;
            }
        }
    }
    leaf.trim_end_matches(|c: char| c.is_ascii_digit())
}

/// EBS volume id from the NVMe serial, which Nitro sets to `vol<hex>` --
/// available without any AWS API call. Returns None for non-EBS devices.
fn ebs_volume_id_for(device: &str) -> Option<String> {
    let serial = std::fs::read_to_string(format!("/sys/block/{device}/device/serial"))
        .ok()
        .or_else(|| {
            std::fs::read_to_string(format!(
                "/sys/class/nvme/{}/serial",
                device.trim_end_matches(|c: char| c == 'n' || c.is_ascii_digit(),)
            ))
            .ok()
        })?;
    let s = serial.trim();
    // Nitro writes "vol0abc..." with the hyphen elided.
    let rest = s.strip_prefix("vol")?;
    if rest.is_empty() || !rest.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    Some(format!("vol-{rest}"))
}

/// Classify what the device physically is. EBS wins when the serial says so;
/// otherwise Amazon's instance-store NVMe identifies itself by model.
fn classify_media(model: &str, is_ebs: bool, rotational: Option<bool>) -> String {
    let m = model.to_ascii_lowercase();
    if is_ebs || m.contains("elastic block store") {
        "ebs".to_string()
    } else if m.contains("instance storage") || m.contains("ec2 nvme instance") {
        "instance-store-nvme".to_string()
    } else if rotational == Some(true) {
        "rotational".to_string()
    } else if m.contains("nvme") {
        "local-nvme".to_string()
    } else if rotational == Some(false) {
        "local-disk".to_string()
    } else {
        "unknown".to_string()
    }
}

/// Provisioned (type, iops, throughput) from the EC2 API. Requires the
/// instance profile to allow ec2:DescribeVolumes; returns None when it does
/// not, so the caller can say "unavailable" instead of inventing a number.
fn describe_ebs_volume(volume_id: &str) -> Option<(String, Option<u32>, Option<u32>)> {
    let out = command_output(
        "aws",
        &[
            "ec2",
            "describe-volumes",
            "--volume-ids",
            volume_id,
            "--query",
            "Volumes[0].[VolumeType,Iops,Throughput]",
            "--output",
            "text",
        ],
    )?;
    let mut f = out.split_whitespace();
    let vtype = f.next()?.to_string();
    let parse = |s: Option<&str>| s.filter(|v| *v != "None").and_then(|v| v.parse().ok());
    Some((vtype, parse(f.next()), parse(f.next())))
}

#[cfg(test)]
mod storage_tests {
    use super::*;

    const MOUNTS: &str = "\
/dev/nvme0n1p1 / ext4 rw,relatime 0 0
/dev/nvme1n1 /mnt/nvme xfs rw,relatime 0 0
tmpfs /run tmpfs rw 0 0
";

    #[test]
    fn st_dev_decoding_handles_majors_above_255() {
        // glibc splits both major and minor across two bit ranges, so a
        // single shift is wrong. nvme's major is 259, which overflows the
        // low 12-bit field -- the case that matters here, since PGDATA on
        // EC2 sits on an nvme device.
        let decode = |dev: u64| {
            let major = ((dev >> 8) & 0xfff) | ((dev >> 32) & !0xfffu64);
            let minor = (dev & 0xff) | ((dev >> 12) & !0xffu64);
            (major, minor)
        };
        // nvme0n1p1 is 259:1 -> glibc packs that as 0x10301.
        assert_eq!(decode(0x10301), (259, 1));
        // sda1 is 8:1.
        assert_eq!(decode(0x801), (8, 1));
        // An anonymous device (btrfs/zfs/overlay) has major 0; /sys/dev/block
        // has no entry, so the caller falls back to /proc/mounts.
        assert_eq!(decode(0x1f).0, 0);
    }

    #[test]
    fn mount_lookup_prefers_the_longest_prefix() {
        // PGDATA on the instance store must resolve to the instance-store
        // mount, not to `/` -- the whole point is catching a data directory
        // that is not where it was meant to be.
        let (mnt, dev, fs) = mount_for_path("/mnt/nvme/pgdata", MOUNTS).unwrap();
        assert_eq!(
            (mnt.as_str(), dev.as_str(), fs.as_str()),
            ("/mnt/nvme", "/dev/nvme1n1", "xfs")
        );

        let (mnt, dev, _) = mount_for_path("/var/lib/postgresql/18/main", MOUNTS).unwrap();
        assert_eq!((mnt.as_str(), dev.as_str()), ("/", "/dev/nvme0n1p1"));
    }

    #[test]
    fn partition_suffix_is_stripped_to_the_parent_device() {
        // /sys/block describes whole devices, so nvme0n1p1 -> nvme0n1.
        assert_eq!(strip_partition_suffix("nvme0n1p1"), "nvme0n1");
        assert_eq!(strip_partition_suffix("mmcblk0p2"), "mmcblk0");
        assert_eq!(strip_partition_suffix("sda2"), "sda");
        assert_eq!(strip_partition_suffix("vdb"), "vdb");
        // A bare nvme namespace has no partition to strip; the trailing
        // digits are part of the device name.
        assert_eq!(strip_partition_suffix("nvme0n1"), "nvme0n1");
        assert_eq!(strip_partition_suffix("nvme10n2"), "nvme10n2");
    }

    #[test]
    fn media_classification_distinguishes_ebs_from_instance_store() {
        assert_eq!(
            classify_media("Amazon Elastic Block Store", true, Some(false)),
            "ebs"
        );
        // Model alone is enough when the serial did not parse.
        assert_eq!(
            classify_media("Amazon Elastic Block Store", false, Some(false)),
            "ebs"
        );
        assert_eq!(
            classify_media("Amazon EC2 NVMe Instance Storage", false, Some(false)),
            "instance-store-nvme"
        );
        assert_eq!(
            classify_media("ST4000DM004", false, Some(true)),
            "rotational"
        );
        assert_eq!(classify_media("", false, None), "unknown");
    }
}
