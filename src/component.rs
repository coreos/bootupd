/*
 * Copyright (C) 2020 Red Hat, Inc.
 *
 * SPDX-License-Identifier: Apache-2.0
 */

use anyhow::{Context, Result};
use cap_std::fs::{Dir, PermissionsExt};
use cap_std::{ambient_authority, fs::Permissions};
use cap_std_ext::dirext::CapStdExtDirExt;
use fn_error_context::context;
use serde::{Deserialize, Serialize};
use std::any::Any;
use std::path::{Path, PathBuf};

use bootc_internal_blockdev::Device;

#[cfg(efi_arch)]
use crate::cli::bootupd::DefaultBootloaderOpts;
use crate::{bootloader::Bootloader, bootupd::RootContext, model::*};

#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum ValidationResult {
    Valid,
    Skip,
    Errors(Vec<String>),
}

#[derive(PartialEq, Eq, Clone, Copy)]
pub(crate) enum ComponentType {
    Bios,
    Efi,
}

impl From<ComponentType> for &'static str {
    fn from(val: ComponentType) -> Self {
        match val {
            ComponentType::Bios => "BIOS",
            ComponentType::Efi => "EFI",
        }
    }
}

impl std::fmt::Display for ComponentType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str((*self).into())
    }
}

/// A bootloader subsystem (EFI or BIOS) that can be installed, updated, and validated.
///
/// Components encapsulate platform-specific bootloader management. Each implementation
/// handles installing bootloader files during image builds, applying updates at runtime,
/// and optionally adopting existing installations not originally managed by bootupd.
pub(crate) trait Component {
    fn as_any(&self) -> &dyn Any;

    /// Returns the name of the component; this will be used for serialization
    /// and should remain stable.
    fn name(&self) -> &'static str;

    /// Returns the type of the component as an enum
    /// Prefer this over [`Component::name`]
    fn component_type(&self) -> ComponentType;

    /// In an operating system whose initially booted disk image is not
    /// using bootupd, detect whether it looks like the component exists
    /// and "synthesize" content metadata from it.
    fn query_adopt(&self, devices: &Option<Vec<Device>>) -> Result<Option<Adoptable>>;

    // Backup the current grub config, and install static grub config from tree
    fn migrate_static_grub_config(&self, sysroot_path: &str, destdir: &Dir) -> Result<()>;

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
        device: Option<&Device>,
        update_firmware: bool,
        bootloader: Bootloader,
    ) -> Result<InstalledContent>;

    /// Implementation of `bootupd generate-update-metadata` for a given component.
    /// This expects to be run during an "image update build" process.  For CoreOS
    /// this is an `rpm-ostree compose tree` for example.  For a dual-partition
    /// style updater, this would be run as part of a postprocessing step
    /// while the filesystem for the partition is mounted.
    fn generate_update_metadata(&self, sysroot: &str) -> Result<Option<ContentMetadata>>;

    /// Used on the client to query for an update cached in the current booted OS.
    fn query_update(
        &self,
        sysroot: &Dir,
        bootloader: Bootloader,
    ) -> Result<Option<ContentMetadata>>;

    /// This is called in the update code if query_update() returned no metadata.
    /// It should return an error if the current booted system should expect some
    /// metadata for this component.
    fn query_requires_update(&self, sysroot: &Dir) -> Result<()>;

    /// Used on the client to run an update.
    fn run_update(
        &self,
        rootcxt: &RootContext,
        current: &InstalledContent,
    ) -> Result<InstalledContent>;

    /// Used on the client to validate an installed version.
    fn validate(&self, current: &InstalledContent, device: &Device) -> Result<ValidationResult>;

    /// Locating efi vendor dir
    fn get_efi_vendor(&self, sysroot: &Path) -> Result<Option<String>>;

    fn is_bootloader_supported(&self, bootloader: Bootloader) -> bool;

    /// Given a component, return metadata on the available update (if any)
    //
    /// If bootloader is Some, all metadata not pertaining to the specified bootloader
    /// is filtered
    ///
    /// If bootloader is None, no filtering is performed
    #[context("Loading update for component {}", self.name())]
    fn get_component_update(
        &self,
        sysroot: &Dir,
        bootloader: Option<Bootloader>,
    ) -> Result<Option<ContentMetadata>> {
        let name = self.component_update_data_name();
        let path = Path::new(BOOTUPD_UPDATES_DIR).join(&name);

        let Some(f) = sysroot.open_optional(&path)? else {
            return Ok(None);
        };

        let mut f = std::io::BufReader::new(f);
        let mut u = serde_json::from_reader(&mut f)
            .with_context(|| format!("failed to parse {:?}", &path))?;

        let Some(bootloader) = bootloader else {
            return Ok(Some(u));
        };

        // We store metadata of all bootloaders present in the image
        // So here, we will now filter out the bootloaders
        u.filter_bootloader(bootloader);

        Ok(Some(u))
    }

    /// Returns the name of the JSON file containing a component's available update metadata installed
    /// into the booted operating system root.
    fn component_update_data_name(&self) -> PathBuf {
        Path::new(&format!("{}.json", self.name())).into()
    }

    #[cfg(efi_arch)]
    fn set_default_bootloader(&self, opts: &DefaultBootloaderOpts) -> Result<()> {
        if !self.is_bootloader_supported(opts.bootloader) {
            anyhow::bail!("{} not supported for {}", opts.bootloader, self.name());
        }

        let root_path = opts.sysroot.as_deref().unwrap_or("/");

        let root = Dir::open_ambient_dir(root_path, ambient_authority())
            .with_context(|| format!("Opening {root_path}"))?;

        // This command expects bootupd.json to be present
        let mut update_meta = self
            .get_component_update(&root, None)?
            .ok_or_else(|| anyhow::anyhow!("Expected to get update metadata"))?;

        if !update_meta.is_bootloader_available(opts.bootloader) {
            anyhow::bail!("{} is not present in metadata", opts.bootloader);
        }

        update_meta.default_bootloader = Some(opts.bootloader);

        write_update_metadata(root_path, self.component_update_data_name(), &update_meta)?;

        Ok(())
    }

    #[cfg(efi_arch)]
    fn get_default_bootloader(&self, root: &Dir) -> Result<Option<Bootloader>> {
        let update_meta = self
            .get_component_update(&root, None)?
            .ok_or_else(|| anyhow::anyhow!("Expected to get update metadata"))?;

        Ok(update_meta.default_bootloader)
    }
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

/// Helper method for writing an update file
pub(crate) fn write_update_metadata(
    sysroot: &str,
    file_path: PathBuf,
    meta: &ContentMetadata,
) -> Result<()> {
    let sysroot = Dir::open_ambient_dir(sysroot, ambient_authority())?;
    let dir = sysroot.open_dir(BOOTUPD_UPDATES_DIR)?;

    dir.atomic_write_with_perms(
        file_path,
        serde_json::to_vec(&meta).context("Serializing metadata")?,
        Permissions::from_mode(0o644),
    )?;

    Ok(())
}

#[context("Querying adoptable state")]
pub(crate) fn query_adopt_state() -> Result<Option<Adoptable>> {
    // This would be extended with support for other operating systems later
    if let Some(aleph) = crate::aleph::get_aleph_version(Path::new("/"))? {
        let meta = ContentMetadata {
            timestamp: aleph.ts,
            version: aleph.aleph.version.unwrap_or_else(|| "unknown".into()),
            versions: None,
            #[cfg(efi_arch)]
            default_bootloader: None,
        };
        log::trace!("Adoptable: {:?}", &meta);
        return Ok(Some(Adoptable {
            version: meta,
            confident: true,
        }));
    } else {
        log::trace!("No aleph version file detected");
    }
    let ostree_deploy_dir = Path::new("/ostree/deploy");
    if ostree_deploy_dir.exists() {
        let btime = ostree_deploy_dir.metadata()?.created()?;
        let timestamp = chrono::DateTime::from(btime);
        let meta = ContentMetadata {
            timestamp,
            version: "unknown".to_string(),
            versions: None,
            #[cfg(efi_arch)]
            default_bootloader: None,
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
    use cap_std::fs::{DirBuilder, DirBuilderExt, Permissions, PermissionsExt};

    use super::*;

    #[test]
    fn test_get_efi_vendor() -> Result<()> {
        let td = tempfile::tempdir()?;
        let tdp = td.path();
        let tdupdates = "usr/lib/bootupd/updates/EFI";
        let tdir = Dir::open_ambient_dir(tdp, ambient_authority())?;

        let mut dir_builder = DirBuilder::new();
        dir_builder.mode(0o755);
        dir_builder.recursive(true);

        tdir.create_dir_with(tdupdates, &dir_builder)?;
        let efi = tdir.open_dir(tdupdates)?;
        efi.create_dir_with("BOOT", &dir_builder)?;
        efi.create_dir_with("fedora", &dir_builder)?;
        efi.create_dir_with("centos", &dir_builder)?;

        efi.atomic_write_with_perms(
            format!("fedora/{}", crate::efi::SHIM),
            "shim data".as_bytes(),
            Permissions::from_mode(0o644),
        )?;

        efi.atomic_write_with_perms(
            format!("centos/{}", crate::efi::SHIM),
            "shim data".as_bytes(),
            Permissions::from_mode(0o644),
        )?;

        let all_components = crate::bootupd::get_components();
        let target_components: Vec<_> = all_components.values().collect();
        for &component in target_components.iter() {
            if component.component_type() == ComponentType::Bios {
                assert_eq!(component.get_efi_vendor(tdp)?, None);
            }
            if component.component_type() == ComponentType::Efi {
                let x = component.get_efi_vendor(tdp);
                assert_eq!(x.is_err(), true);
                efi.remove_dir_all("centos")?;
                assert_eq!(component.get_efi_vendor(tdp)?, Some("fedora".to_string()));
                {
                    let td_vendor = "usr/lib/efi/shim/15.8-3/EFI/centos";
                    tdir.create_dir_with(td_vendor, &dir_builder)?;
                    let shim_dir = tdir.open_dir(td_vendor)?;

                    shim_dir.atomic_write_with_perms(
                        crate::efi::SHIM,
                        "shim data".as_bytes(),
                        Permissions::from_mode(0o644),
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
                    tdir.remove_dir_all("usr/lib/efi")?;
                    tdir.remove_dir_all(tdupdates)?;
                    let err = component.get_efi_vendor(&td_usr).unwrap_err();
                    assert_eq!(err.to_string(), "Failed to find valid target path");
                }
            }
        }
        Ok(())
    }

    #[test]
    #[cfg(efi_arch)]
    fn test_set_default_bootloader() -> Result<()> {
        use chrono::Utc;

        let td = tempfile::tempdir()?;
        let sysroot = td.path().to_str().unwrap().to_string();
        let tdir = Dir::open_ambient_dir(&sysroot, ambient_authority())?;

        // Create the updates directory
        let mut dir_builder = DirBuilder::new();
        dir_builder.mode(0o755);
        dir_builder.recursive(true);
        tdir.create_dir_with(BOOTUPD_UPDATES_DIR, &dir_builder)?;

        // Create test metadata without the target bootloader
        let meta = ContentMetadata {
            timestamp: Utc::now(),
            version: "grub2-efi-x64-1:2.12-21.fc41.x86_64".into(), // Only has grub2-efi, not grub-cc
            versions: None,
            default_bootloader: None,
        };

        let all_components = crate::bootupd::get_components();
        let efi_component = all_components.get("EFI").unwrap();

        // Write metadata file
        write_update_metadata(&sysroot, efi_component.component_update_data_name(), &meta)?;

        let opts = DefaultBootloaderOpts {
            sysroot: Some(sysroot.clone()),
            bootloader: Bootloader::GrubCC, // This bootloader is not in the metadata version string
        };

        if efi_component.is_bootloader_supported(opts.bootloader) {
            let result = efi_component.set_default_bootloader(&opts);
            assert!(result.is_err());
            assert!(result
                .unwrap_err()
                .to_string()
                .contains("is not present in metadata"));
        }

        // Now create test metadata with both bootloaders available
        let meta = ContentMetadata {
            timestamp: Utc::now(),
            version: "grub2-efi-x64-1:2.12-21.fc41.x86_64,grub-cc-efi-x64-1:2.12-21.fc41.x86_64"
                .into(),
            versions: None,
            default_bootloader: None,
        };

        // Write initial metadata file
        write_update_metadata(&sysroot, efi_component.component_update_data_name(), &meta)?;

        let opts = DefaultBootloaderOpts {
            sysroot: Some(sysroot.clone()),
            bootloader: Bootloader::GrubCC,
        };

        if efi_component.is_bootloader_supported(opts.bootloader) {
            // Should succeed
            let result = efi_component.set_default_bootloader(&opts);
            assert!(result.is_ok());

            // Verify the metadata was updated
            let root = Dir::open_ambient_dir(&sysroot, ambient_authority())?;
            let updated_meta = efi_component.get_component_update(&root, None)?;
            assert!(updated_meta.is_some());
            let updated_meta = updated_meta.unwrap();
            assert_eq!(updated_meta.default_bootloader, Some(Bootloader::GrubCC));
        }

        Ok(())
    }
}
