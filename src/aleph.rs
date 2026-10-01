//! Aleph version file support.
//!
//! The "aleph" file records the original OS version at install time.
//! This is written by CoreOS (`.coreos-aleph-version.json`) and
//! bootc (`.bootc-aleph.json`).

/*
 * Copyright (C) 2020 Red Hat, Inc.
 *
 * SPDX-License-Identifier: Apache-2.0
 */

use anyhow::{Context, Result};
use chrono::prelude::*;
use fn_error_context::context;
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::path::Path;

#[derive(Serialize, Deserialize, Clone, Debug, Hash, Ord, PartialOrd, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
/// See https://github.com/coreos/fedora-coreos-tracker/blob/66d7d00bedd9d5eabc7287b9577f443dcefb7c04/internals/README-internals.md#aleph-version
pub(crate) struct Aleph {
    #[serde(alias = "build")]
    pub(crate) version: String,
}

pub(crate) struct AlephWithTimestamp {
    pub(crate) aleph: Aleph,
    #[allow(dead_code)]
    pub(crate) ts: chrono::DateTime<Utc>,
}

/// Known aleph file paths, checked in order of priority.
const ALEPH_PATHS: &[&str] = &[
    "sysroot/.coreos-aleph-version.json",
    "sysroot/.bootc-aleph.json",
];

#[context("Getting aleph version")]
pub(crate) fn get_aleph_version(root: &Path) -> Result<Option<AlephWithTimestamp>> {
    for aleph_path in ALEPH_PATHS {
        let path = &root.join(aleph_path);
        if !path.exists() {
            continue;
        }
        let statusf = File::open(path).with_context(|| format!("Opening {path:?}"))?;
        let meta = statusf.metadata()?;
        let bufr = std::io::BufReader::new(statusf);
        let aleph: Aleph =
            serde_json::from_reader(bufr).with_context(|| format!("Parsing {path:?}"))?;
        log::debug!("Found aleph version in {aleph_path}");
        return Ok(Some(AlephWithTimestamp {
            aleph,
            ts: meta.created()?.into(),
        }));
    }
    Ok(None)
}

#[cfg(test)]
mod test {
    use super::*;
    use anyhow::Result;

    const V1_ALEPH_DATA: &str = r##"
    {
        "version": "32.20201002.dev.2",
        "ref": "fedora/x86_64/coreos/testing-devel",
        "ostree-commit": "b2ea6159d6274e1bbbb49aa0ef093eda5d53a75c8a793dbe184f760ed64dc862"
    }"##;

    // Waiting on https://github.com/rust-lang/rust/pull/125692
    #[cfg(not(target_env = "musl"))]
    #[test]
    fn test_parse_from_root_empty() -> Result<()> {
        // Verify we're a no-op in an empty root
        let root: &tempfile::TempDir = &tempfile::tempdir()?;
        let root = root.path();
        assert!(get_aleph_version(root).unwrap().is_none());
        Ok(())
    }

    // Waiting on https://github.com/rust-lang/rust/pull/125692
    #[cfg(not(target_env = "musl"))]
    #[test]
    fn test_parse_from_root_coreos() -> Result<()> {
        let root: &tempfile::TempDir = &tempfile::tempdir()?;
        let root = root.path();
        let sysroot = &root.join("sysroot");
        std::fs::create_dir(sysroot).context("Creating sysroot")?;
        std::fs::write(root.join(ALEPH_PATHS[0]), V1_ALEPH_DATA).context("Writing aleph")?;
        let aleph = get_aleph_version(root).unwrap().unwrap();
        assert_eq!(aleph.aleph.version, "32.20201002.dev.2");
        Ok(())
    }

    // Waiting on https://github.com/rust-lang/rust/pull/125692
    #[cfg(not(target_env = "musl"))]
    #[test]
    fn test_parse_from_root_bootc() -> Result<()> {
        let root: &tempfile::TempDir = &tempfile::tempdir()?;
        let root = root.path();
        let sysroot = &root.join("sysroot");
        std::fs::create_dir(sysroot).context("Creating sysroot")?;
        std::fs::write(root.join(ALEPH_PATHS[1]), V1_ALEPH_DATA).context("Writing aleph")?;
        let aleph = get_aleph_version(root).unwrap().unwrap();
        assert_eq!(aleph.aleph.version, "32.20201002.dev.2");
        Ok(())
    }

    // Waiting on https://github.com/rust-lang/rust/pull/125692
    #[cfg(not(target_env = "musl"))]
    #[test]
    fn test_parse_coreos_preferred_over_bootc() -> Result<()> {
        // When both files exist, the CoreOS aleph should be preferred
        let root: &tempfile::TempDir = &tempfile::tempdir()?;
        let root = root.path();
        let sysroot = &root.join("sysroot");
        std::fs::create_dir(sysroot).context("Creating sysroot")?;
        let coreos_data = r##"{"version": "coreos-version"}"##;
        let bootc_data = r##"{"version": "bootc-version"}"##;
        std::fs::write(root.join(ALEPH_PATHS[0]), coreos_data).context("Writing coreos aleph")?;
        std::fs::write(root.join(ALEPH_PATHS[1]), bootc_data).context("Writing bootc aleph")?;
        let aleph = get_aleph_version(root).unwrap().unwrap();
        assert_eq!(aleph.aleph.version, "coreos-version");
        Ok(())
    }

    // Waiting on https://github.com/rust-lang/rust/pull/125692
    #[cfg(not(target_env = "musl"))]
    #[test]
    fn test_parse_from_root_linked() -> Result<()> {
        let root: &tempfile::TempDir = &tempfile::tempdir()?;
        let root = root.path();
        let sysroot = &root.join("sysroot");
        std::fs::create_dir(sysroot).context("Creating sysroot")?;
        let target_name = ".new-ostree-aleph.json";
        let target = &sysroot.join(target_name);
        std::fs::write(root.join(target), V1_ALEPH_DATA).context("Writing aleph")?;
        std::os::unix::fs::symlink(target_name, root.join(ALEPH_PATHS[0])).context("Symlinking")?;
        let aleph = get_aleph_version(root).unwrap().unwrap();
        assert_eq!(aleph.aleph.version, "32.20201002.dev.2");
        Ok(())
    }

    #[test]
    fn test_parse_old_aleph() -> Result<()> {
        // What the aleph file looked like before we changed it in
        // https://github.com/osbuild/osbuild/pull/1475
        let alephdata = r##"
{
    "build": "32.20201002.dev.2",
    "ref": "fedora/x86_64/coreos/testing-devel",
    "ostree-commit": "b2ea6159d6274e1bbbb49aa0ef093eda5d53a75c8a793dbe184f760ed64dc862",
    "imgid": "fedora-coreos-32.20201002.dev.2-qemu.x86_64.qcow2"
}"##;
        let aleph: Aleph = serde_json::from_str(alephdata)?;
        assert_eq!(aleph.version, "32.20201002.dev.2");
        Ok(())
    }

    #[test]
    fn test_parse_aleph() -> Result<()> {
        let aleph: Aleph = serde_json::from_str(V1_ALEPH_DATA)?;
        assert_eq!(aleph.version, "32.20201002.dev.2");
        Ok(())
    }

    #[test]
    fn test_parse_bootc_aleph() -> Result<()> {
        // A realistic bootc aleph as written by `bootc install`.
        // See https://github.com/bootc-dev/bootc/commit/c6112d2d2fa3b785c858741f1585b0578088eda2
        let alephdata = r##"
{
    "digest": "sha256:07bf537cc4e4d208eb0b978f76e5046e55529ce6192b982d8c1a41fa1d61b95a",
    "kernel": "6.18.13-200.fc43.x86_64",
    "labels": {
        "com.coreos.inputhash": "fe9883169714c593d98058606e886b9747710ed15ab1b9cdbd7fa538fb435b3c",
        "com.coreos.osname": "fedora-coreos",
        "com.coreos.stream": "testing-devel",
        "containers.bootc": "1",
        "io.buildah.version": "1.42.2",
        "org.opencontainers.image.description": "Fedora CoreOS testing-devel",
        "org.opencontainers.image.revision": "233fe18749c7d2749581e4307c4cac60967acde4",
        "org.opencontainers.image.source": "git@github.com:jbtrystram/fedora-coreos-config.git",
        "org.opencontainers.image.title": "Fedora CoreOS testing-devel",
        "org.opencontainers.image.version": "43.20260301.20.dev1",
        "ostree.bootable": "1",
        "ostree.commit": "89635f7cba9de932fc60d71a6bded65ad0db06a35c9d016da03ca7ade9ba4736",
        "ostree.final-diffid": "sha256:12787d84fa137cd5649a9005efe98ec9d05ea46245fdc50aecb7dd007f2035b1"
    },
    "selinux": "disabled",
    "target-image": "ostree-image-signed:docker://quay.io/fedora/fedora-coreos:testing-devel",
    "timestamp": null,
    "version": "43.20260301.20.dev1"
}"##;
        let aleph: Aleph = serde_json::from_str(alephdata)?;
        assert_eq!(aleph.version, "43.20260301.20.dev1");
        Ok(())
    }
}
