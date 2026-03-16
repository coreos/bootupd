/*
 * Copyright (C) 2020 Red Hat, Inc.
 *
 * SPDX-License-Identifier: Apache-2.0
 */

use std::cell::RefCell;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use bootc_internal_utils::CommandRunExt;
use camino::{Utf8Path, Utf8PathBuf};
use cap_std::fs::Dir;
use cap_std_ext::cap_std;
use cap_std_ext::dirext::CapStdExtDirExt;
use chrono::prelude::*;
use fn_error_context::context;
use openat_ext::OpenatDirExt;
use os_release::OsRelease;
use rustix::{fd::AsFd, fd::BorrowedFd, fs::StatVfsMountFlags};
use walkdir::WalkDir;
use widestring::U16CString;

use crate::bootupd::RootContext;
use crate::freezethaw::fsfreeze_thaw_cycle;
use crate::model::*;
use crate::ostreeutil;
use crate::util;
use crate::{blockdev, filetree, grubconfigs};
use crate::{component::*, packagesystem::*};

/// Well-known paths to the ESP that may have been mounted external to us.
pub(crate) const ESP_MOUNTS: &[&str] = &["boot/efi", "efi", "boot"];

/// New efi dir under usr/lib
const EFILIB: &str = "usr/lib/efi";

/// The binary to change EFI boot ordering
const EFIBOOTMGR: &str = "efibootmgr";
#[cfg(target_arch = "aarch64")]
pub(crate) const SHIM: &str = "shimaa64.efi";

#[cfg(target_arch = "x86_64")]
pub(crate) const SHIM: &str = "shimx64.efi";

#[cfg(target_arch = "riscv64")]
pub(crate) const SHIM: &str = "shimriscv64.efi";

/// The mount path for uefi
const EFIVARFS: &str = "/sys/firmware/efi/efivars";

/// Systemd boot loader info EFI variable names
const LOADER_INFO_VAR_STR: &str = "LoaderInfo-4a67b082-0a4c-41cf-b6c7-440b29bb8c4f";
const STUB_INFO_VAR_STR: &str = "StubInfo-4a67b082-0a4c-41cf-b6c7-440b29bb8c4f";

/// The options of cp command for installation
const OPTIONS: &[&str] = &["-rp", "--reflink=auto"];

/// Return `true` if the system is booted via EFI
pub(crate) fn is_efi_booted() -> Result<bool> {
    Path::new("/sys/firmware/efi")
        .try_exists()
        .map_err(Into::into)
}

#[derive(Default)]
pub(crate) struct Efi {
    mountpoint: RefCell<Option<PathBuf>>,
}

impl Efi {
    // Get mounted point for esp
    pub(crate) fn get_mounted_esp(&self, root: &Path) -> Result<Option<PathBuf>> {
        // First check all potential mount points without holding the borrow
        let mut found_mount = None;
        for &mnt in ESP_MOUNTS.iter() {
            let path = root.join(mnt);
            if !path.exists() {
                continue;
            }

            let st = rustix::fs::statfs(&path)?;
            if st.f_type == libc::MSDOS_SUPER_MAGIC {
                util::ensure_writable_mount(&path)?;
                found_mount = Some(path);
                break;
            }
        }

        // Only borrow mutably if we found a mount point
        if let Some(mnt) = found_mount {
            log::debug!("Reusing existing mount point {mnt:?}");
            *self.mountpoint.borrow_mut() = Some(mnt.clone());
            Ok(Some(mnt))
        } else {
            Ok(None)
        }
    }

    // Mount the passed esp_device, return mount point
    pub(crate) fn mount_esp_device(&self, root: &Path, esp_device: &Path) -> Result<PathBuf> {
        let mut mountpoint = None;

        for &mnt in ESP_MOUNTS.iter() {
            let mnt = root.join(mnt);
            if !mnt.exists() {
                continue;
            }
            std::process::Command::new("mount")
                .arg(&esp_device)
                .arg(&mnt)
                .run_inherited()
                .with_context(|| format!("Failed to mount {:?}", esp_device))?;
            log::debug!("Mounted at {mnt:?}");
            mountpoint = Some(mnt);
            break;
        }
        let mnt = mountpoint.ok_or_else(|| anyhow::anyhow!("No mount point found"))?;
        *self.mountpoint.borrow_mut() = Some(mnt.clone());
        Ok(mnt)
    }

    // Firstly check if esp is already mounted, then mount the passed esp device
    pub(crate) fn ensure_mounted_esp(&self, root: &Path, esp_device: &Path) -> Result<PathBuf> {
        if let Some(mountpoint) = self.mountpoint.borrow().as_deref() {
            return Ok(mountpoint.to_owned());
        }
        let destdir = if let Some(destdir) = self.get_mounted_esp(Path::new(root))? {
            destdir
        } else {
            self.mount_esp_device(root, esp_device)?
        };
        Ok(destdir)
    }

    fn unmount(&self) -> Result<()> {
        if let Some(mount) = self.mountpoint.borrow_mut().take() {
            Command::new("umount")
                .arg(&mount)
                .run_inherited()
                .with_context(|| format!("Failed to unmount {mount:?}"))?;
            log::trace!("Unmounted");
        }
        Ok(())
    }

    #[context("Updating EFI firmware variables")]
    fn update_firmware(&self, device: &str, espdir: &openat::Dir, vendordir: &str) -> Result<()> {
        if !is_efi_booted()? {
            log::debug!("Not booted via EFI, skipping firmware update");
            return Ok(());
        }
        let sysroot = Dir::open_ambient_dir("/", cap_std::ambient_authority())?;
        let efi = sysroot
            .open_dir(EFIVARFS.strip_prefix("/").unwrap())
            .context("Opening efivars dir")?;
        let st = rustix::fs::fstatvfs(efi.as_fd())?;
        // Do nothing if efivars is readonly or empty
        // See https://github.com/coreos/bootupd/issues/972
        if st.f_flag.contains(StatVfsMountFlags::RDONLY)
            || std::fs::read_dir(EFIVARFS)?.next().is_none()
        {
            log::info!("Skipped EFI variables update: efivars not writable or empty");
            return Ok(());
        }

        // Check shim exists and return earlier if not
        let shim_path = format!("EFI/{vendordir}/{SHIM}");
        if !espdir.exists(&shim_path)? {
            anyhow::bail!("Failed to find {shim_path}");
        }
        let loader = format!("\\EFI\\{vendordir}\\{SHIM}");

        let product_name = get_product_name(&sysroot)?;
        log::debug!("Get product name: '{product_name}'");
        assert!(product_name.len() > 0);

        let esp_part_num = blockdev::get_esp_partition_number(device)?;

        // clear all the boot entries that match the target name
        clear_efi_target(&product_name)?;
        create_efi_boot_entry(device, esp_part_num.trim(), &loader, &product_name)
    }
}

#[context("Get product name")]
fn get_product_name(sysroot: &Dir) -> Result<String> {
    let release_path = "etc/system-release";
    if sysroot.exists(release_path) {
        let content = sysroot.read_to_string(release_path)?;
        let re = regex::Regex::new(r" *release.*").unwrap();
        let name = re.replace_all(&content, "").trim().to_string();
        return Ok(name);
    }
    // Read /etc/os-release
    let release: OsRelease = OsRelease::new()?;
    Ok(release.name)
}

/// Convert a nul-terminated UTF-16 byte array to a String.
fn string_from_utf16_bytes(slice: &[u8]) -> String {
    // For some reason, systemd appends 3 nul bytes after the string.
    // Drop the last byte if there's an odd number.
    let size = slice.len() / 2;
    let v: Vec<u16> = (0..size)
        .map(|i| u16::from_ne_bytes([slice[2 * i], slice[2 * i + 1]]))
        .collect();
    U16CString::from_vec(v).unwrap().to_string_lossy()
}

/// Read a nul-terminated UTF-16 string from an EFI variable.
fn read_efi_var_utf16_string(name: &str) -> Option<String> {
    let efivars = Path::new(EFIVARFS);
    if !efivars.exists() {
        log::trace!("No efivars mount at {:?}", efivars);
        return None;
    }
    let path = efivars.join(name);
    if !path.exists() {
        log::trace!("No EFI variable {name}");
        return None;
    }
    match std::fs::read(&path) {
        Ok(buf) => {
            // Skip the first 4 bytes, those are the EFI variable attributes.
            if buf.len() < 4 {
                log::warn!("Read less than 4 bytes from {:?}", path);
                return None;
            }
            Some(string_from_utf16_bytes(&buf[4..]))
        }
        Err(reason) => {
            log::warn!("Failed reading {:?}: {reason}", path);
            None
        }
    }
}

/// Read the LoaderInfo EFI variable if it exists.
fn get_loader_info() -> Option<String> {
    read_efi_var_utf16_string(LOADER_INFO_VAR_STR)
}

/// Read the StubInfo EFI variable if it exists.
fn get_stub_info() -> Option<String> {
    read_efi_var_utf16_string(STUB_INFO_VAR_STR)
}

/// Whether to skip adoption if a systemd bootloader is found.
fn skip_systemd_bootloaders() -> bool {
    if let Some(loader_info) = get_loader_info() {
        if loader_info.starts_with("systemd") {
            log::trace!("Skipping adoption for {:?}", loader_info);
            return true;
        }
    }
    if let Some(stub_info) = get_stub_info() {
        log::trace!("Skipping adoption for {:?}", stub_info);
        return true;
    }
    false
}

impl Component for Efi {
    fn name(&self) -> &'static str {
        "EFI"
    }

    fn query_adopt(&self, devices: &Option<Vec<String>>) -> Result<Option<Adoptable>> {
        if devices.is_none() {
            log::trace!("No ESP detected");
            return Ok(None);
        };

        // Don't adopt if the system is booted with systemd-boot or
        // systemd-stub since those will be managed with bootctl.
        if skip_systemd_bootloaders() {
            return Ok(None);
        }
        crate::component::query_adopt_state()
    }

    // Backup "/boot/efi/EFI/{vendor}/grub.cfg" to "/boot/efi/EFI/{vendor}/grub.cfg.bak"
    // Replace "/boot/efi/EFI/{vendor}/grub.cfg" with new static "grub.cfg"
    fn migrate_static_grub_config(&self, sysroot_path: &str, destdir: &openat::Dir) -> Result<()> {
        let sysroot =
            openat::Dir::open(sysroot_path).with_context(|| format!("Opening {sysroot_path}"))?;
        let Some(vendor) = self.get_efi_vendor(&Path::new(sysroot_path))? else {
            anyhow::bail!("Failed to find efi vendor");
        };

        // destdir is /boot/efi/EFI
        let efidir = destdir
            .sub_dir(&vendor)
            .with_context(|| format!("Opening EFI/{}", vendor))?;

        if !efidir.exists(grubconfigs::GRUBCONFIG_BACKUP)? {
            println!("Creating a backup of the current GRUB config on EFI");
            efidir
                .copy_file(grubconfigs::GRUBCONFIG, grubconfigs::GRUBCONFIG_BACKUP)
                .context("Failed to backup GRUB config")?;
        }

        grubconfigs::install(&sysroot, None, Some(&vendor), true)?;
        // Synchronize the filesystem containing /boot/efi/EFI/{vendor} to disk.
        fsfreeze_thaw_cycle(efidir.open_file(".")?)?;

        Ok(())
    }

    /// Given an adoptable system and an update, perform the update.
    fn adopt_update(
        &self,
        rootcxt: &RootContext,
        updatemeta: &ContentMetadata,
        with_static_config: bool,
    ) -> Result<Option<InstalledContent>> {
        let esp_devices = blockdev::find_colocated_esps(&rootcxt.devices)?;
        let Some(meta) = self.query_adopt(&esp_devices)? else {
            return Ok(None);
        };

        let updated_path = {
            let efilib_path = rootcxt.path.join(EFILIB);
            if efilib_path.exists() && get_efi_component_from_usr(&rootcxt.path, EFILIB)?.is_some()
            {
                PathBuf::from(EFILIB)
            } else {
                component_updatedirname(self)
            }
        };
        let updated = rootcxt
            .sysroot
            .sub_dir(&updated_path)
            .with_context(|| format!("opening update dir {}", updated_path.display()))?;
        let updatef = filetree::FileTree::new_from_dir(&updated).context("reading update dir")?;

        let esp_devices = esp_devices.unwrap_or_default();
        for esp in esp_devices {
            let destpath = &self.ensure_mounted_esp(rootcxt.path.as_ref(), Path::new(&esp))?;

            let efidir = openat::Dir::open(&destpath.join("EFI")).context("opening EFI dir")?;
            validate_esp_fstype(&efidir)?;

            // For adoption, we should only touch files that we know about.
            let diff = updatef.relative_diff_to(&efidir)?;
            log::trace!("applying adoption diff: {}", &diff);
            filetree::apply_diff(&updated, &efidir, &diff, None)
                .context("applying filesystem changes")?;

            // Backup current config and install static config
            if with_static_config {
                // Install the static config if the OSTree bootloader is not set.
                if let Some(bootloader) = crate::ostreeutil::get_ostree_bootloader()? {
                    println!(
                        "ostree repo 'sysroot.bootloader' config option is currently set to: '{bootloader}'",
                    );
                } else {
                    println!("ostree repo 'sysroot.bootloader' config option is not set yet");
                    self.migrate_static_grub_config(rootcxt.path.as_str(), &efidir)?;
                };
            }

            // Do the sync before unmount
            fsfreeze_thaw_cycle(efidir.open_file(".")?)?;
            drop(efidir);
            self.unmount().context("unmount after adopt")?;
        }
        Ok(Some(InstalledContent {
            meta: updatemeta.clone(),
            filetree: Some(updatef),
            adopted_from: Some(meta.version),
        }))
    }

    fn install(
        &self,
        src_root: &str,
        dest_root: &str,
        device: &str,
        update_firmware: bool,
    ) -> Result<InstalledContent> {
        let src_dir = openat::Dir::open(src_root)
            .with_context(|| format!("opening source directory {src_root}"))?;
        let Some(meta) = get_component_update(&src_dir, self)? else {
            anyhow::bail!("No update metadata for component {} found", self.name());
        };
        log::debug!("Found metadata {}", meta.version);

        // Let's attempt to use an already mounted ESP at the target
        // dest_root if one is already mounted there in a known ESP location.
        let destpath = if let Some(destdir) = self.get_mounted_esp(Path::new(dest_root))? {
            destdir
        } else {
            // Using `blockdev` to find the partition instead of partlabel because
            // we know the target install toplevel device already.
            if device.is_empty() {
                anyhow::bail!("Device value not provided");
            }
            let esp_device = blockdev::get_esp_partition(device)?
                .ok_or_else(|| anyhow::anyhow!("Failed to find ESP device"))?;
            self.mount_esp_device(Path::new(dest_root), Path::new(&esp_device))?
        };

        let destd = &openat::Dir::open(&destpath)
            .with_context(|| format!("opening dest dir {}", destpath.display()))?;
        validate_esp_fstype(destd)?;

        let src_path = Utf8Path::new(src_root);
        let efi_comps = if src_path.join(EFILIB).exists() {
            get_efi_component_from_usr(&src_path, EFILIB)?
        } else {
            None
        };
        let dest = destpath.to_str().with_context(|| {
            format!(
                "Include invalid UTF-8 characters in dest {}",
                destpath.display()
            )
        })?;

        let efi_path = if let Some(efi_components) = efi_comps {
            for efi in efi_components {
                filetree::copy_dir_with_args(&src_dir, efi.path.as_str(), dest, OPTIONS)?;
            }
            EFILIB
        } else {
            let updates = component_updatedirname(self);
            let src = updates
                .to_str()
                .context("Include invalid UTF-8 characters in path")?;
            filetree::copy_dir_with_args(&src_dir, src, dest, OPTIONS)?;
            &src.to_owned()
        };

        // Get filetree from efi path
        let ft = crate::filetree::FileTree::new_from_dir(&src_dir.sub_dir(efi_path)?)?;
        if update_firmware {
            if let Some(vendordir) = self.get_efi_vendor(src_path.join(efi_path).as_std_path())? {
                self.update_firmware(device, destd, &vendordir)?
            }
        }
        Ok(InstalledContent {
            meta,
            filetree: Some(ft),
            adopted_from: None,
        })
    }

    fn run_update(
        &self,
        rootcxt: &RootContext,
        current: &InstalledContent,
    ) -> Result<InstalledContent> {
        let currentf = current
            .filetree
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("No filetree for installed EFI found!"))?;
        let sysroot_dir = &rootcxt.sysroot;
        let updatemeta = self.query_update(sysroot_dir)?.expect("update available");
        let updated_path = {
            let efilib_path = rootcxt.path.join(EFILIB);
            if efilib_path.exists() && get_efi_component_from_usr(&rootcxt.path, EFILIB)?.is_some()
            {
                PathBuf::from(EFILIB)
            } else {
                component_updatedirname(self)
            }
        };

        let updated = rootcxt
            .sysroot
            .sub_dir(&updated_path)
            .with_context(|| format!("opening update dir {}", updated_path.display()))?;
        let updatef = filetree::FileTree::new_from_dir(&updated).context("reading update dir")?;
        let diff = currentf.diff(&updatef)?;

        let Some(esp_devices) = blockdev::find_colocated_esps(&rootcxt.devices)? else {
            anyhow::bail!("Failed to find all esp devices");
        };

        // Get old vendor name from installed filetree
        let old_vendor = {
            let mut vendor = None;
            for child in currentf.children.keys() {
                if child.ends_with(SHIM) {
                    vendor = child.strip_suffix(&format!("/{SHIM}"));
                    break;
                }
            }
            let Some(vendor) = vendor else {
                anyhow::bail!("Failed to find current vendor");
            };
            vendor.to_owned()
        };

        // Get new vendor name
        let new_vendor = {
            let sysroot = rootcxt.path.as_std_path();
            let Some(vendor) = self.get_efi_vendor(&sysroot.join(&updated_path))? else {
                anyhow::bail!("Failed to find efi vendor in new update");
            };
            vendor.to_owned()
        };

        for esp in esp_devices {
            let destpath = &self.ensure_mounted_esp(rootcxt.path.as_ref(), Path::new(&esp))?;
            let dest_efi = destpath.join("EFI");
            let destdir = openat::Dir::open(&dest_efi).context("opening EFI dir")?;
            validate_esp_fstype(&destdir)?;

            log::trace!("applying diff: {}", &diff);
            filetree::apply_diff(&updated, &destdir, &diff, None)
                .context("applying filesystem changes")?;

            // Install grub.cfg under new vendor
            if new_vendor != old_vendor {
                grubconfigs::install(sysroot_dir, None, Some(&new_vendor), true)?;
                destdir.remove_all(&old_vendor)?;
            }

            // Do the sync before unmount
            fsfreeze_thaw_cycle(destdir.open_file(".")?)?;
            drop(destdir);
            self.unmount().context("unmount after update")?;
        }

        let adopted_from = None;
        Ok(InstalledContent {
            meta: updatemeta,
            filetree: Some(updatef),
            adopted_from,
        })
    }

    fn generate_update_metadata(&self, sysroot: &str) -> Result<Option<ContentMetadata>> {
        let sysroot_path = Path::new(sysroot);
        let sysroot_dir = Dir::open_ambient_dir(sysroot_path, cap_std::ambient_authority())?;

        if let Some(ostreeboot) = sysroot_dir
            .open_dir_optional(ostreeutil::BOOT_PREFIX)
            .context("Opening usr/lib/ostree-boot")?
        {
            let cruft = ["loader", "grub2"];
            for p in cruft.iter() {
                ostreeboot.remove_all_optional(p)?;
            }
            // Transfer ostree-boot EFI files to usr/lib/efi
            transfer_ostree_boot_to_usr(sysroot_path)?;

            // Remove usr/lib/ostree-boot/efi/EFI dir (after transfer) or if it is empty
            ostreeboot.remove_all_optional("efi/EFI")?;
        }

        if let Some(efi_components) =
            get_efi_component_from_usr(Utf8Path::from_path(sysroot_path).unwrap(), EFILIB)?
        {
            let mut packages = Vec::new();
            let mut modules_vec: Vec<Module> = vec![];
            for efi in efi_components {
                packages.push(format!("{}-{}", efi.name, efi.version));
                modules_vec.push(Module {
                    name: efi.name,
                    rpm_evr: efi.version,
                });
            }
            modules_vec.sort_unstable();

            // change to now to workaround https://github.com/coreos/bootupd/issues/933
            let timestamp = std::time::SystemTime::now();
            let meta = ContentMetadata {
                timestamp: chrono::DateTime::<Utc>::from(timestamp),
                version: packages.join(","),
                versions: Some(modules_vec),
            };
            write_update_metadata(sysroot, self, &meta)?;
            Ok(Some(meta))
        } else {
            anyhow::bail!("Failed to find EFI components");
        }
    }

    fn query_update(&self, sysroot: &openat::Dir) -> Result<Option<ContentMetadata>> {
        let content_metadata = get_component_update(sysroot, self)?;
        // Failed as expected if booted with EFI and no update metadata
        if content_metadata.is_none() && is_efi_booted()? {
            anyhow::bail!("Failed to find EFI update metadata");
        }
        Ok(content_metadata)
    }

    fn validate(&self, current: &InstalledContent) -> Result<ValidationResult> {
        let devices = crate::blockdev::get_devices("/").context("get parent devices")?;
        let esp_devices = blockdev::find_colocated_esps(&devices)?;
        if !is_efi_booted()? && esp_devices.is_none() {
            return Ok(ValidationResult::Skip);
        }
        let currentf = current
            .filetree
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("No filetree for installed EFI found!"))?;

        let mut errs = Vec::new();
        let esp_devices = esp_devices.unwrap_or_default();
        for esp in esp_devices.iter() {
            let destpath = &self.ensure_mounted_esp(Path::new("/"), Path::new(&esp))?;

            let efidir = openat::Dir::open(&destpath.join("EFI"))
                .with_context(|| format!("opening EFI dir {}", destpath.display()))?;
            let diff = currentf.relative_diff_to(&efidir)?;

            for f in diff.changes.iter() {
                errs.push(format!("Changed: {}", f));
            }
            for f in diff.removals.iter() {
                errs.push(format!("Removed: {}", f));
            }
            assert_eq!(diff.additions.len(), 0);
            drop(efidir);
            self.unmount().context("unmount after validate")?;
        }

        if !errs.is_empty() {
            Ok(ValidationResult::Errors(errs))
        } else {
            Ok(ValidationResult::Valid)
        }
    }

    fn get_efi_vendor(&self, sysroot: &Path) -> Result<Option<String>> {
        let efi_lib = sysroot.join(EFILIB);
        let updates = sysroot.join(component_updatedirname(self));

        let paths: [&Path; 3] = [&efi_lib, &updates, sysroot];
        let target = paths
            .into_iter()
            .find(|p| p.exists())
            .ok_or_else(|| anyhow::anyhow!("Failed to find valid target path"))?;
        let shim_files = find_file_recursive(target, SHIM)?;

        // Does not support multiple shim for efi
        if shim_files.len() > 1 {
            anyhow::bail!("Found multiple {SHIM} in the image: {:?}", shim_files);
        }
        if let Some(p) = shim_files.first() {
            let p = p
                .parent()
                .unwrap()
                .file_name()
                .ok_or_else(|| anyhow::anyhow!("No file name found"))?;
            Ok(Some(p.to_string_lossy().into_owned()))
        } else {
            anyhow::bail!("Failed to find {SHIM} in the image")
        }
    }
}

impl Drop for Efi {
    fn drop(&mut self) {
        log::debug!("Unmounting");
        let _ = self.unmount();
    }
}

fn validate_esp_fstype(dir: &openat::Dir) -> Result<()> {
    let dir = unsafe { BorrowedFd::borrow_raw(dir.as_raw_fd()) };
    let stat = rustix::fs::fstatfs(&dir)?;
    if stat.f_type != libc::MSDOS_SUPER_MAGIC {
        bail!(
            "EFI mount is not a msdos filesystem, but is {:?}",
            stat.f_type
        );
    };
    Ok(())
}

#[derive(Debug, PartialEq)]
struct BootEntry {
    id: String,
    name: String,
}

/// Parse boot entries from efibootmgr output
fn parse_boot_entries(output: &str) -> Vec<BootEntry> {
    let mut entries = Vec::new();

    for line in output.lines().filter_map(|line| line.strip_prefix("Boot")) {
        // Need to consider if output only has "Boot0000* UiApp", without additional info
        if line.starts_with('0') {
            let parts = if let Some((parts, _)) = line.split_once('\t') {
                parts
            } else {
                line
            };
            if let Some((id, name)) = parts.split_once(' ') {
                let id = id.trim_end_matches('*').to_string();
                let name = name.trim().to_string();
                entries.push(BootEntry { id, name });
            }
        }
    }
    entries
}

#[context("Clearing EFI boot entries that match target {target}")]
pub(crate) fn clear_efi_target(target: &str) -> Result<()> {
    let target = target.to_lowercase();
    let output = Command::new(EFIBOOTMGR).output()?;
    if !output.status.success() {
        anyhow::bail!("Failed to invoke {EFIBOOTMGR}")
    }

    let output = String::from_utf8(output.stdout)?;
    let boot_entries = parse_boot_entries(&output);
    for entry in boot_entries {
        if entry.name.to_lowercase() == target {
            log::debug!("Deleting matched target {:?}", entry);
            let mut cmd = Command::new(EFIBOOTMGR);
            cmd.args(["-b", entry.id.as_str(), "-B"]);
            println!("Executing: {cmd:?}");
            cmd.run_inherited_with_cmd_context()?;
        }
    }

    anyhow::Ok(())
}

#[context("Adding new EFI boot entry")]
pub(crate) fn create_efi_boot_entry(
    device: &str,
    esp_partition_number: &str,
    loader: &str,
    target: &str,
) -> Result<()> {
    log::debug!("Creating new EFI boot entry using '{target}'");
    let mut cmd = Command::new(EFIBOOTMGR);
    cmd.args([
        "--create",
        "--disk",
        device,
        "--part",
        esp_partition_number,
        "--loader",
        loader,
        "--label",
        target,
    ]);
    println!("Executing: {cmd:?}");
    cmd.run_inherited_with_cmd_context()
}

#[context("Find target file recursively")]
fn find_file_recursive<P: AsRef<Path>>(dir: P, target_file: &str) -> Result<Vec<PathBuf>> {
    let mut result = Vec::new();

    for entry in WalkDir::new(dir).into_iter().filter_map(|e| e.ok()) {
        if entry.file_type().is_file() {
            if let Some(file_name) = entry.file_name().to_str() {
                if file_name == target_file {
                    if let Some(path) = entry.path().to_str() {
                        result.push(path.into());
                    }
                }
            }
        }
    }

    Ok(result)
}

#[derive(Debug, PartialEq, Eq)]
pub struct EFIComponent {
    pub name: String,
    pub version: String,
    path: Utf8PathBuf,
}

/// Get EFIComponents from e.g. usr/lib/efi, like "usr/lib/efi/<name>/<version>/EFI"
fn get_efi_component_from_usr<'a>(
    sysroot: &'a Utf8Path,
    usr_path: &'a str,
) -> Result<Option<Vec<EFIComponent>>> {
    let efilib_path = sysroot.join(usr_path);
    let skip_count = Utf8Path::new(usr_path).components().count();

    let mut components: Vec<EFIComponent> = WalkDir::new(&efilib_path)
        .min_depth(3) // <name>/<version>/EFI: so 3 levels down
        .max_depth(3)
        .into_iter()
        .filter_map(|entry| {
            let entry = entry.ok()?;
            if !entry.file_type().is_dir() || entry.file_name() != "EFI" {
                return None;
            }

            let abs_path = entry.path();
            let rel_path = abs_path.strip_prefix(sysroot).ok()?;
            let utf8_rel_path = Utf8PathBuf::from_path_buf(rel_path.to_path_buf()).ok()?;

            let mut components = utf8_rel_path.components();

            let name = components.nth(skip_count)?.to_string();
            let version = components.next()?.to_string();

            Some(EFIComponent {
                name,
                version,
                path: utf8_rel_path,
            })
        })
        .collect();

    if components.len() == 0 {
        return Ok(None);
    }
    components.sort_by(|a, b| a.name.cmp(&b.name));

    Ok(Some(components))
}

/// Copy files from usr/lib/ostree-boot/efi/EFI to /usr/lib/efi/<component>/<evr>/
fn transfer_ostree_boot_to_usr(sysroot: &Path) -> Result<()> {
    let ostreeboot_efi = Path::new(ostreeutil::BOOT_PREFIX).join("efi");
    let ostreeboot_efi_path = sysroot.join(&ostreeboot_efi);

    let efi = ostreeboot_efi_path.join("EFI");
    if !efi.exists() {
        return Ok(());
    }
    for entry in WalkDir::new(&efi) {
        let entry = entry?;

        if entry.file_type().is_file() {
            let entry_path = entry.path();

            // get path EFI/{BOOT,<vendor>}/<file>
            let filepath = entry_path.strip_prefix(&ostreeboot_efi_path)?;
            // get path /boot/efi/EFI/{BOOT,<vendor>}/<file>
            let boot_filepath = Path::new("/boot/efi").join(filepath);

            // Run `rpm -qf <filepath>`
            let pkg = crate::packagesystem::query_file(
                sysroot.to_str().unwrap(),
                boot_filepath.to_str().unwrap(),
            )?;

            let (name, evr) = pkg.split_once(' ').unwrap();
            let component = name.split('-').next().unwrap_or("");
            // get path usr/lib/efi/<component>/<evr>
            let efilib_path = Path::new(EFILIB).join(component).join(evr);

            let sysroot_dir = openat::Dir::open(sysroot)?;
            // Ensure dest parent directory exists
            if let Some(parent) = efilib_path.join(filepath).parent() {
                sysroot_dir.ensure_dir_all(parent, 0o755)?;
            }

            // Source dir is usr/lib/ostree-boot/efi
            let src = sysroot_dir
                .sub_dir(&ostreeboot_efi)
                .context("Opening ostree-boot dir")?;
            // Dest dir is usr/lib/efi/<component>/<evr>
            let dest = sysroot_dir
                .sub_dir(&efilib_path)
                .context("Opening usr/lib/efi dir")?;
            // Copy file from ostree-boot to usr/lib/efi
            src.copy_file_at(filepath, &dest, filepath)
                .context("Copying file to usr/lib/efi")?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use cap_std_ext::dirext::CapStdExtDirExt;

    use super::*;

    #[test]
    fn test_parse_boot_entries() -> Result<()> {
        let output = r"
BootCurrent: 0003
Timeout: 0 seconds
BootOrder: 0003,0001,0000,0002
Boot0000* UiApp	FvVol(7cb8bdc9-f8eb-4f34-aaea-3ee4af6516a1)/FvFile(462caa21-7614-4503-836e-8ab6f4662331)
Boot0001* UEFI Misc Device	PciRoot(0x0)/Pci(0x3,0x0){auto_created_boot_option}
Boot0002* EFI Internal Shell	FvVol(7cb8bdc9-f8eb-4f34-aaea-3ee4af6516a1)/FvFile(7c04a583-9e3e-4f1c-ad65-e05268d0b4d1)
Boot0003* Fedora	HD(2,GPT,94ff4025-5276-4bec-adea-e98da271b64c,0x1000,0x3f800)/\EFI\fedora\shimx64.efi";
        let entries = parse_boot_entries(output);
        assert_eq!(
            entries,
            [
                BootEntry {
                    id: "0000".to_string(),
                    name: "UiApp".to_string()
                },
                BootEntry {
                    id: "0001".to_string(),
                    name: "UEFI Misc Device".to_string()
                },
                BootEntry {
                    id: "0002".to_string(),
                    name: "EFI Internal Shell".to_string()
                },
                BootEntry {
                    id: "0003".to_string(),
                    name: "Fedora".to_string()
                }
            ]
        );
        let output = r"
BootCurrent: 0003
Timeout: 0 seconds
BootOrder: 0003,0001,0000,0002";
        let entries = parse_boot_entries(output);
        assert_eq!(entries, []);

        let output = r"
BootCurrent: 0003
Timeout: 0 seconds
BootOrder: 0003,0001,0000,0002
Boot0000* UiApp
Boot0001* UEFI Misc Device
Boot0002* EFI Internal Shell
Boot0003* test";
        let entries = parse_boot_entries(output);
        assert_eq!(
            entries,
            [
                BootEntry {
                    id: "0000".to_string(),
                    name: "UiApp".to_string()
                },
                BootEntry {
                    id: "0001".to_string(),
                    name: "UEFI Misc Device".to_string()
                },
                BootEntry {
                    id: "0002".to_string(),
                    name: "EFI Internal Shell".to_string()
                },
                BootEntry {
                    id: "0003".to_string(),
                    name: "test".to_string()
                }
            ]
        );
        Ok(())
    }
    #[cfg(test)]
    fn fixture() -> Result<cap_std_ext::cap_tempfile::TempDir> {
        let tempdir = cap_std_ext::cap_tempfile::tempdir(cap_std::ambient_authority())?;
        tempdir.create_dir("etc")?;
        Ok(tempdir)
    }
    #[test]
    fn test_get_product_name() -> Result<()> {
        let tmpd = fixture()?;
        {
            tmpd.atomic_write("etc/system-release", "Fedora release 40 (Forty)")?;
            let name = get_product_name(&tmpd)?;
            assert_eq!("Fedora", name);
        }
        {
            tmpd.atomic_write("etc/system-release", "CentOS Stream release 9")?;
            let name = get_product_name(&tmpd)?;
            assert_eq!("CentOS Stream", name);
        }
        {
            tmpd.atomic_write(
                "etc/system-release",
                "Red Hat Enterprise Linux CoreOS release 4",
            )?;
            let name = get_product_name(&tmpd)?;
            assert_eq!("Red Hat Enterprise Linux CoreOS", name);
        }
        {
            tmpd.atomic_write(
                "etc/system-release",
                "Red Hat Enterprise Linux CoreOS release 4
                ",
            )?;
            let name = get_product_name(&tmpd)?;
            assert_eq!("Red Hat Enterprise Linux CoreOS", name);
        }
        {
            tmpd.remove_file("etc/system-release")?;
            let name = get_product_name(&tmpd)?;
            assert!(name.len() > 0);
        }
        Ok(())
    }

    #[test]
    fn test_get_efi_component_from_usr() -> Result<()> {
        let tmpdir: &tempfile::TempDir = &tempfile::tempdir()?;
        let tpath = tmpdir.path();
        let efi_path = tpath.join("usr/lib/efi");
        std::fs::create_dir_all(efi_path.join("BAR/1.1/EFI"))?;
        std::fs::create_dir_all(efi_path.join("FOO/1.1/EFI"))?;
        std::fs::create_dir_all(efi_path.join("FOOBAR/1.1/test"))?;
        let utf8_tpath =
            Utf8Path::from_path(tpath).ok_or_else(|| anyhow::anyhow!("Path is not valid UTF-8"))?;
        let efi_comps = get_efi_component_from_usr(utf8_tpath, EFILIB)?;
        assert_eq!(
            efi_comps,
            Some(vec![
                EFIComponent {
                    name: "BAR".to_string(),
                    version: "1.1".to_string(),
                    path: Utf8PathBuf::from("usr/lib/efi/BAR/1.1/EFI"),
                },
                EFIComponent {
                    name: "FOO".to_string(),
                    version: "1.1".to_string(),
                    path: Utf8PathBuf::from("usr/lib/efi/FOO/1.1/EFI"),
                },
            ])
        );
        std::fs::remove_dir_all(efi_path.join("BAR/1.1/EFI"))?;
        std::fs::remove_dir_all(efi_path.join("FOO/1.1/EFI"))?;
        let efi_comps = get_efi_component_from_usr(utf8_tpath, EFILIB)?;
        assert_eq!(efi_comps, None);
        Ok(())
    }
}
