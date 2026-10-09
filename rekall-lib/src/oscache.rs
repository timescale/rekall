// Copyright (c) 2026 Tiger Data, Inc.
// Licensed under the PostgreSQL License. See LICENSE for details.

//! Host-level cache control for CacheMode::DiskCold.
//!
//! PostgreSQL can evict its own shared buffers but cannot reach the OS
//! page cache, so disk-cold measurement shells out to a host command.
//! rekall always runs on the host it benchmarks (remote runs execute a
//! local rekall on the provisioned instance), so a local command
//! suffices. The default relies on passwordless sudo (the case on
//! infra-provisioned EC2 instances); locked-down hosts can point
//! `cache_drop_command` at a fixed, root-owned helper script that a
//! sudoers entry whitelists — a whitelist must name a fixed trusted
//! path, which is the only reason a script would ever be preferred
//! over the direct command.

/// Run the configured drop-caches command. A failure (permission denied,
/// missing binary, non-zero exit) is returned as a message rather than
/// aborting the run: the caller logs it and records that the point ran
/// with shared buffers evicted only, so reports state what actually
/// happened instead of overclaiming disk-cold.
pub fn drop_os_caches(command: &str) -> std::result::Result<(), String> {
    let out = match std::process::Command::new("sh")
        .arg("-c")
        .arg(command)
        .output()
    {
        Ok(o) => o,
        Err(e) => return Err(format!("could not execute `{command}`: {e}")),
    };
    if !out.status.success() {
        return Err(format!(
            "`{command}` failed ({}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim(),
        ));
    }
    Ok(())
}
