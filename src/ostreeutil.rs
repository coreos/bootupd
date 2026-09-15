/*
 * Copyright (C) 2020 Red Hat, Inc.
 *
 * SPDX-License-Identifier: Apache-2.0
 */

use anyhow::{Context, Result};

/// https://github.com/coreos/rpm-ostree/pull/969/commits/dc0e8db5bd92e1f478a0763d1a02b48e57022b59
#[cfg(any(
    target_arch = "x86_64",
    target_arch = "aarch64",
    target_arch = "riscv64"
))]
pub(crate) const BOOT_PREFIX: &str = "usr/lib/ostree-boot";

/// Get sysroot.bootloader in ostree repo config.
pub(crate) fn get_ostree_bootloader() -> Result<Option<String>> {
    let mut cmd = std::process::Command::new("ostree");
    let result = cmd
        .args([
            "config",
            "--repo=/sysroot/ostree/repo",
            "get",
            "sysroot.bootloader",
        ])
        .output()
        .context("Querying ostree sysroot.bootloader")?;
    if !result.status.success() {
        // ostree will exit with a none zero return code if the key does not exists
        return Ok(None);
    } else {
        let res = String::from_utf8(result.stdout)
            .with_context(|| "decoding as UTF-8 output of ostree command")?;
        let bootloader = res.trim_end().to_string();
        return Ok(Some(bootloader));
    }
}

pub(crate) fn set_ostree_bootloader(bootloader: &str) -> Result<()> {
    let status = std::process::Command::new("ostree")
        .args([
            "config",
            "--repo=/sysroot/ostree/repo",
            "set",
            "sysroot.bootloader",
            bootloader,
        ])
        .status()?;
    if !status.success() {
        anyhow::bail!("Failed to set 'sysroot.bootloader' to '{bootloader}' in ostree repo config");
    }
    Ok(())
}
