/*
 * Copyright (C) 2020 Red Hat, Inc.
 *
 * SPDX-License-Identifier: Apache-2.0
 */

use anyhow::{Context, Result};
use fn_error_context::context;
use openat_ext::OpenatDirExt;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use crate::{bootupd::RootContext, model::*};

#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum ValidationResult {
    Valid,
    Skip,
    Errors(Vec<String>),
}

/// A component along with a possible update
pub(crate) trait Component {
    /// Returns the name of the component; this will be used for serialization
    /// and should remain stable.
    fn name(&self) -> &'static str;

    /// In an operating system whose initially booted disk image is not
    /// using bootupd, detect whether it looks like the component exists
    /// and "synthesize" content metadata from it.
    fn query_adopt(&self, devices: &Option<Vec<String>>) -> Result<Option<Adoptable>>;

    // Backup the current grub config, and install static grub config from tree
    fn migrate_static_grub_config(&self, sysroot_path: &str, destdir: &openat::Dir) -> Result<()>;

    /// Given an adoptable system and an update, perform the update.
    fn adopt_update(
        &self,
        rootcxt: &RootContext,
        update: &ContentMetadata,
        with_static_config: bool,
    ) -> Result<Option<InstalledContent>>;

    /// Implementation of `bootupd install` for a given component.  This should
    /// gather data (or run binaries) from the source root, and install them
    /// into the target root.  It is expected that sub-partitions (e.g. the ESP)
    /// are mounted at the expected place.  For operations that require a block device instead
    /// of a filesystem root, the component should query the mount point to
    /// determine the block device.
    /// This will be run during a disk image build process.
    fn install(
        &self,
        src_root: &str,
        dest_root: &str,
        device: &str,
        update_firmware: bool,
    ) -> Result<InstalledContent>;

    /// Implementation of `bootupd generate-update-metadata` for a given component.
    /// This expects to be run during an "image update build" process.  For CoreOS
    /// this is an `rpm-ostree compose tree` for example.  For a dual-partition
    /// style updater, this would be run as part of a postprocessing step
    /// while the filesystem for the partition is mounted.
    fn generate_update_metadata(&self, sysroot: &str) -> Result<Option<ContentMetadata>>;

    /// Used on the client to query for an update cached in the current booted OS.
    fn query_update(&self, sysroot: &openat::Dir) -> Result<Option<ContentMetadata>>;

    /// Used on the client to run an update.
    fn run_update(
        &self,
        rootcxt: &RootContext,
        current: &InstalledContent,
    ) -> Result<InstalledContent>;

    /// Used on the client to validate an installed version.
    fn validate(&self, current: &InstalledContent) -> Result<ValidationResult>;

    /// Locating efi vendor dir
    fn get_efi_vendor(&self, sysroot: &Path) -> Result<Option<String>>;
}

/// Given a component name, create an implementation.
pub(crate) fn new_from_name(name: &str) -> Result<Box<dyn Component>> {
    Ok(match name {
        #[cfg(any(
            target_arch = "x86_64",
            target_arch = "aarch64",
            target_arch = "riscv64"
        ))]
        #[allow(clippy::box_default)]
        "EFI" => Box::new(crate::efi::Efi::default()),
        #[cfg(any(target_arch = "x86_64", target_arch = "powerpc64"))]
        #[allow(clippy::box_default)]
        "BIOS" => Box::new(crate::bios::Bios::default()),
        _ => anyhow::bail!("No component {}", name),
    })
}

/// Returns the path to the payload directory for an available update for
/// a component.
#[cfg(any(
    target_arch = "x86_64",
    target_arch = "aarch64",
    target_arch = "riscv64"
))]
pub(crate) fn component_updatedirname(component: &dyn Component) -> PathBuf {
    Path::new(BOOTUPD_UPDATES_DIR).join(component.name())
}

/// Returns the path to the payload directory for an available update for
/// a component.
#[allow(dead_code)]
#[cfg(any(
    target_arch = "x86_64",
    target_arch = "aarch64",
    target_arch = "riscv64"
))]
pub(crate) fn component_updatedir(sysroot: &str, component: &dyn Component) -> PathBuf {
    Path::new(sysroot).join(component_updatedirname(component))
}

/// Returns the name of the JSON file containing a component's available update metadata installed
/// into the booted operating system root.
fn component_update_data_name(component: &dyn Component) -> PathBuf {
    Path::new(&format!("{}.json", component.name())).into()
}

/// Helper method for writing an update file
pub(crate) fn write_update_metadata(
    sysroot: &str,
    component: &dyn Component,
    meta: &ContentMetadata,
) -> Result<()> {
    let sysroot = openat::Dir::open(sysroot)?;
    let dir = sysroot.sub_dir(BOOTUPD_UPDATES_DIR)?;
    let name = component_update_data_name(component);
    dir.write_file_with(name, 0o644, |w| -> Result<_> {
        Ok(serde_json::to_writer(w, &meta)?)
    })?;
    Ok(())
}

/// Given a component, return metadata on the available update (if any)
#[context("Loading update for component {}", component.name())]
pub(crate) fn get_component_update(
    sysroot: &openat::Dir,
    component: &dyn Component,
) -> Result<Option<ContentMetadata>> {
    let name = component_update_data_name(component);
    let path = Path::new(BOOTUPD_UPDATES_DIR).join(name);
    if let Some(f) = sysroot.open_file_optional(&path)? {
        let mut f = std::io::BufReader::new(f);
        let u = serde_json::from_reader(&mut f)
            .with_context(|| format!("failed to parse {:?}", &path))?;
        Ok(Some(u))
    } else {
        Ok(None)
    }
}

#[context("Querying adoptable state")]
pub(crate) fn query_adopt_state() -> Result<Option<Adoptable>> {
    // This would be extended with support for other operating systems later
    if let Some(coreos_aleph) = crate::coreos::get_aleph_version(Path::new("/"))? {
        let meta = ContentMetadata {
            timestamp: coreos_aleph.ts,
            version: coreos_aleph.aleph.version,
            versions: None,
        };
        log::trace!("Adoptable: {:?}", &meta);
        return Ok(Some(Adoptable {
            version: meta,
            confident: true,
        }));
    } else {
        log::trace!("No CoreOS aleph detected");
    }
    let ostree_deploy_dir = Path::new("/ostree/deploy");
    if ostree_deploy_dir.exists() {
        let btime = ostree_deploy_dir.metadata()?.created()?;
        let timestamp = chrono::DateTime::from(btime);
        let meta = ContentMetadata {
            timestamp,
            version: "unknown".to_string(),
            versions: None,
        };
        return Ok(Some(Adoptable {
            version: meta,
            confident: true,
        }));
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_get_efi_vendor() -> Result<()> {
        let td = tempfile::tempdir()?;
        let tdp = td.path();
        let tdupdates = "usr/lib/bootupd/updates/EFI";
        let tdir = openat::Dir::open(tdp)?;

        tdir.ensure_dir_all(tdupdates, 0o755)?;
        let efi = tdir.sub_dir(tdupdates)?;
        efi.create_dir("BOOT", 0o755)?;
        efi.create_dir("fedora", 0o755)?;
        efi.create_dir("centos", 0o755)?;

        efi.write_file_contents(
            format!("fedora/{}", crate::efi::SHIM),
            0o644,
            "shim data".as_bytes(),
        )?;
        efi.write_file_contents(
            format!("centos/{}", crate::efi::SHIM),
            0o644,
            "shim data".as_bytes(),
        )?;

        let all_components = crate::bootupd::get_components();
        let target_components: Vec<_> = all_components.values().collect();
        for &component in target_components.iter() {
            if component.name() == "BIOS" {
                assert_eq!(component.get_efi_vendor(tdp)?, None);
            }
            if component.name() == "EFI" {
                let x = component.get_efi_vendor(tdp);
                assert_eq!(x.is_err(), true);
                efi.remove_all("centos")?;
                assert_eq!(component.get_efi_vendor(tdp)?, Some("fedora".to_string()));
                {
                    let td_vendor = "usr/lib/efi/shim/15.8-3/EFI/centos";
                    tdir.ensure_dir_all(td_vendor, 0o755)?;
                    let shim_dir = tdir.sub_dir(td_vendor)?;
                    shim_dir.write_file_contents(
                        crate::efi::SHIM,
                        0o644,
                        "shim data".as_bytes(),
                    )?;

                    // usr/lib/efi wins and get 'centos'
                    assert_eq!(component.get_efi_vendor(tdp)?, Some("centos".to_string()));
                    // find directly from usr/lib/efi and get 'centos'
                    let td_usr = tdp.join("usr/lib/efi");
                    assert_eq!(
                        component.get_efi_vendor(&td_usr)?,
                        Some("centos".to_string())
                    );
                    // find directly from updates and get 'fedora'
                    let td_efi = tdp.join(component_updatedirname(&**component));
                    assert_eq!(
                        component.get_efi_vendor(&td_efi)?,
                        Some("fedora".to_string())
                    );
                    tdir.remove_all("usr/lib/efi")?;
                    tdir.remove_all(tdupdates)?;
                    let err = component.get_efi_vendor(&td_usr).unwrap_err();
                    assert_eq!(err.to_string(), "Failed to find valid target path");
                }
            }
        }
        Ok(())
    }
}
