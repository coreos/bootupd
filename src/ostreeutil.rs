/*
 * Copyright (C) 2020 Red Hat, Inc.
 *
 * SPDX-License-Identifier: Apache-2.0
 */

use std::path::Path;

use anyhow::{Context, Result};
use log::debug;

/// https://github.com/coreos/rpm-ostree/pull/969/commits/dc0e8db5bd92e1f478a0763d1a02b48e57022b59
#[cfg(any(
    target_arch = "x86_64",
    target_arch = "aarch64",
    target_arch = "riscv64"
))]
pub(crate) const BOOT_PREFIX: &str = "usr/lib/ostree-boot";
const LEGACY_RPMOSTREE_DBPATH: &str = "usr/share/rpm";
const SYSIMAGE_RPM_DBPATH: &str = "usr/lib/sysimage/rpm";

/// Returns true if the target directory contains at least one file that does
/// not start with `.`
fn is_nonempty_dir(path: impl AsRef<Path>) -> Result<bool> {
    let path = path.as_ref();
    let it = match std::fs::read_dir(path) {
        Ok(r) => r,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e.into()),
    };
    for ent in it {
        let ent = ent?;
        let name = ent.file_name();
        if name.as_encoded_bytes().starts_with(b".") {
            continue;
        }
        return Ok(true);
    }
    Ok(false)
}

pub(crate) fn rpm_cmd<P: AsRef<Path>>(sysroot: P) -> Result<std::process::Command> {
    let mut c = std::process::Command::new("rpm");
    let sysroot = sysroot.as_ref();
    // Take the first non-empty database path
    let mut arg = None;
    for dbpath in [SYSIMAGE_RPM_DBPATH, LEGACY_RPMOSTREE_DBPATH] {
        let dbpath = sysroot.join(dbpath);
        if !is_nonempty_dir(&dbpath)? {
            continue;
        }
        let mut s = std::ffi::OsString::new();
        s.push("--dbpath=");
        s.push(dbpath.as_os_str());
        arg = Some(s);
        break;
    }
    if let Some(arg) = arg {
        debug!("Using dbpath {arg:?}");
        c.arg(arg);
    } else {
        debug!("Failed to find dbpath");
    }
    Ok(c)
}

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
