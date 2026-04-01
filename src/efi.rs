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

use bootc_internal_blockdev::Device;

use crate::bootupd::RootContext;
use crate::freezethaw::fsfreeze_thaw_cycle;
use crate::model::*;
use crate::ostreeutil;
use crate::util;
use uapi_version::Version;

use crate::{component::*, packagesystem::*};
use crate::{filetree, grubconfigs};

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

/// Check if the given path is a mount point via statx(MOUNT_ROOT).
fn is_mount_point(path: &Path) -> Result<bool> {
    use rustix::fs::{AtFlags, StatxAttributes, StatxFlags};
    // See https://github.com/coreos/cap-std-ext/blob/5493d689/src/dirext.rs#L514
    // CWD is unused here because callers always pass absolute paths.
    let r = rustix::fs::statx(
        rustix::fs::CWD,
        path,
        AtFlags::NO_AUTOMOUNT | AtFlags::SYMLINK_NOFOLLOW,
        StatxFlags::empty(),
    )?;
    if r.stx_attributes_mask.contains(StatxAttributes::MOUNT_ROOT) {
        Ok(r.stx_attributes.contains(StatxAttributes::MOUNT_ROOT))
    } else {
        anyhow::bail!(
            "could not determine if {path:?} is a mount point (kernel too old for MOUNT_ROOT)"
        )
    }
}

/// Copy options that merge source contents into an existing destination
/// directory instead of creating a subdirectory (used for non-EFI
/// components like firmware that live at the ESP root).
const OPTIONS_MERGE: &[&str] = &["-rp", "--reflink=auto", "-T"];

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
                // Verify this is actually a mount point, not just a subdirectory
                // on a vfat filesystem. Compare device IDs: a mount point has a
                // different device than its parent. Without this check, a vfat
                // subdirectory (e.g. /boot/efi on a vfat /boot) could be
                // misidentified as a mounted ESP.
                if !is_mount_point(&path)? {
                    // Same device as parent - this is a subdirectory, not a mount point
                    log::debug!("Skipping {path:?}: vfat but not a mount point");
                    continue;
                }
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

            // Check if the target is already a mounted ESP (e.g. the host
            // already has the ESP mounted when running install-to-filesystem).
            let st = rustix::fs::statfs(&mnt)?;
            if st.f_type == libc::MSDOS_SUPER_MAGIC {
                if is_mount_point(&mnt)? {
                    log::debug!("ESP already mounted at {mnt:?}, reusing");
                    mountpoint = Some(mnt);
                    break;
                }
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
    fn update_firmware(
        &self,
        device: &Device,
        espdir: &openat::Dir,
        vendordir: &str,
    ) -> Result<()> {
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

        // Multi-device is handled by the caller: Efi::install() is called once
        // per device in the loop at bootupd.rs install(), so each device's ESP
        // gets its firmware updated individually.
        let esp_part_num = device.get_esp_partition_number()?;

        // clear all the boot entries that match the target name
        clear_efi_target(&product_name)?;
        let device_path = device.path();
        create_efi_boot_entry(&device_path, esp_part_num.trim(), &loader, &product_name)
    }

    fn ensure_efi_prefix(&self, mut ft: filetree::FileTree) -> filetree::FileTree {
        let needs_prefix =
            !ft.children.is_empty() && ft.children.keys().all(|k| !k.starts_with("EFI/"));
        if needs_prefix {
            ft.prepend_prefix("EFI");
        }
        ft
    }

    /// Build a `FileTree` from either the new `usr/lib/efi/` layout (when
    /// pre-resolved components are provided) or the legacy
    /// `usr/lib/bootupd/updates/EFI` directory.
    ///
    /// For the new layout, only the latest version of each component is
    /// included so that duplicate destination keys cannot arise.
    fn build_filetree(
        &self,
        root_dir: &openat::Dir,
        components: Option<&[EFIComponent]>,
    ) -> Result<(PathBuf, filetree::FileTree)> {
        if let Some(components) = components {
            let p = PathBuf::from(EFILIB);
            let dir = root_dir
                .sub_dir(&p)
                .with_context(|| format!("opening {}", p.display()))?;
            let latest = latest_versions(components);
            let prefixes: Vec<String> = latest
                .iter()
                .map(|c| format!("{}/{}", c.name, c.version))
                .collect();
            let ft = filetree::FileTree::new_from_dir_strip_prefix_for(&dir, &prefixes)?;
            Ok((p, ft))
        } else {
            let p = component_updatedirname(self);
            let dir = root_dir
                .sub_dir(&p)
                .with_context(|| format!("opening {}", p.display()))?;
            let mut ft = filetree::FileTree::new_from_dir(&dir).context("reading update dir")?;
            ft = self.ensure_efi_prefix(ft);
            Ok((p, ft))
        }
    }

    /// Build the update `FileTree`, resolving EFI components from the
    /// sysroot and delegating to `build_filetree`.
    fn build_update_filetree(
        &self,
        sysroot: &openat::Dir,
        sysroot_path: &Utf8Path,
    ) -> Result<(PathBuf, filetree::FileTree)> {
        let efilib_path = sysroot_path.join(EFILIB);
        let components = if efilib_path.exists() {
            get_efi_component_from_usr(sysroot_path, EFILIB)?
        } else {
            None
        };
        self.build_filetree(sysroot, components.as_deref())
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

    fn query_adopt(&self, devices: &Option<Vec<Device>>) -> Result<Option<Adoptable>> {
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

        // destdir is /boot/efi
        let efidir = destdir
            .sub_dir(&format!("EFI/{}", vendor))
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
        let esp_devices = rootcxt.device.find_colocated_esps()?;
        let Some(meta) = self.query_adopt(&esp_devices)? else {
            return Ok(None);
        };

        let (updated_path, updatef) =
            self.build_update_filetree(&rootcxt.sysroot, &rootcxt.path)?;
        let updated = rootcxt
            .sysroot
            .sub_dir(&updated_path)
            .with_context(|| format!("opening update dir {}", updated_path.display()))?;

        let esp_devices = esp_devices.unwrap_or_default();
        for esp in esp_devices {
            let destpath =
                &self.ensure_mounted_esp(rootcxt.path.as_ref(), Path::new(&esp.path()))?;

            let destdir = openat::Dir::open(destpath).context("opening ESP dir")?;
            validate_esp_fstype(&destdir)?;

            // For adoption, we should only touch files that we know about.
            let diff = updatef.relative_diff_to(&destdir)?;
            log::trace!("applying adoption diff: {}", &diff);
            filetree::apply_diff(&updated, &destdir, &diff, None)
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
                    self.migrate_static_grub_config(rootcxt.path.as_str(), &destdir)?;
                };
            }

            // Do the sync before unmount
            fsfreeze_thaw_cycle(destdir.open_file(".")?)?;
            drop(destdir);
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
        device: Option<&Device>,
        update_firmware: bool,
    ) -> Result<InstalledContent> {
        let src_dir = openat::Dir::open(src_root)
            .with_context(|| format!("opening source directory {src_root}"))?;
        let Some(meta) = get_component_update(&src_dir, self)? else {
            anyhow::bail!("No update metadata for component {} found", self.name());
        };
        log::debug!("Found metadata {}", meta.version);

        // Determine the destination path for the ESP.
        // If a device is provided, find and mount its ESP partition (unmounting
        // any previously mounted ESP first to target the correct device).
        // If no device is provided, fall back to an already-mounted ESP.
        let destpath = if let Some(dev) = device {
            // Unmount any previously mounted ESP to ensure we install to the
            // correct device. This is important for multi-device installs where
            // each device has its own ESP.
            self.unmount()?;

            let esp_device = dev
                .find_partition_of_esp()
                .with_context(|| format!("Failed to find ESP device on {}", dev.path()))?;
            self.mount_esp_device(Path::new(dest_root), Path::new(&esp_device.path()))?
        } else if let Some(destdir) = self.get_mounted_esp(Path::new(dest_root))? {
            destdir
        } else {
            anyhow::bail!("No device specified and no mounted ESP found");
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

        // Copy files to the ESP
        if let Some(ref efi_components) = efi_comps {
            for efi in efi_components {
                if efi.has_efi_subdir {
                    filetree::copy_dir_with_args(&src_dir, efi.path.as_str(), dest, OPTIONS)?;
                } else {
                    filetree::copy_dir_with_args(&src_dir, efi.path.as_str(), dest, OPTIONS_MERGE)?;
                }
            }
        } else {
            let updates = component_updatedirname(self);
            let src = updates
                .to_str()
                .context("Include invalid UTF-8 characters in path")?;
            filetree::copy_dir_with_args(&src_dir, src, dest, OPTIONS)?;
        };

        // Build the filetree from the update source
        let (update_path, ft) = self.build_filetree(&src_dir, efi_comps.as_deref())?;
        let efi_vendor_search = src_path.as_std_path().join(update_path);

        if update_firmware {
            if let Some(dev) = device {
                if let Some(vendordir) = self.get_efi_vendor(efi_vendor_search.as_path())? {
                    self.update_firmware(dev, destd, &vendordir)?
                }
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
        let mut currentf = current
            .filetree
            .clone()
            .ok_or_else(|| anyhow::anyhow!("No filetree for installed EFI found!"))?;
        currentf = self.ensure_efi_prefix(currentf);
        let sysroot_dir = &rootcxt.sysroot;
        let updatemeta = self.query_update(sysroot_dir)?.expect("update available");

        let (updated_path, updatef) =
            self.build_update_filetree(&rootcxt.sysroot, &rootcxt.path)?;

        let diff = currentf.diff(&updatef)?;

        let updated = rootcxt
            .sysroot
            .sub_dir(&updated_path)
            .with_context(|| format!("opening update dir {}", updated_path.display()))?;

        let Some(esp_devices) = rootcxt.device.find_colocated_esps()? else {
            anyhow::bail!("Failed to find all esp devices");
        };

        for esp in esp_devices {
            let destpath =
                &self.ensure_mounted_esp(rootcxt.path.as_ref(), Path::new(&esp.path()))?;
            let destdir = openat::Dir::open(destpath).context("opening ESP dir")?;
            validate_esp_fstype(&destdir)?;
            log::trace!("applying diff: {}", &diff);
            filetree::apply_diff(&updated, &destdir, &diff, None)
                .context("applying filesystem changes")?;

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
            // Transfer ostree-boot efi/ files to usr/lib/efi
            transfer_ostree_boot_to_usr(sysroot_path)?;

            // Remove the entire efi/ tree after transfer, or if it is empty
            ostreeboot.remove_all_optional("efi")?;
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

    fn validate(&self, current: &InstalledContent, device: &Device) -> Result<ValidationResult> {
        let esp_devices = device.find_colocated_esps()?;
        if !is_efi_booted()? && esp_devices.is_none() {
            return Ok(ValidationResult::Skip);
        }
        let mut currentf = current
            .filetree
            .clone()
            .ok_or_else(|| anyhow::anyhow!("No filetree for installed EFI found!"))?;
        currentf = self.ensure_efi_prefix(currentf);

        let mut errs = Vec::new();
        let esp_devices = esp_devices.unwrap_or_default();
        for esp in esp_devices.iter() {
            let destpath = &self.ensure_mounted_esp(Path::new("/"), Path::new(&esp.path()))?;

            let destdir = openat::Dir::open(destpath)
                .with_context(|| format!("opening ESP dir {}", destpath.display()))?;
            let diff = currentf.relative_diff_to(&destdir)?;

            for f in diff.changes.iter() {
                errs.push(format!("Changed: {}", f));
            }
            for f in diff.removals.iter() {
                errs.push(format!("Removed: {}", f));
            }
            assert_eq!(diff.additions.len(), 0);
            drop(destdir);
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
            anyhow::bail!("Found multiple {SHIM} in the image");
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
    has_efi_subdir: bool,
}

/// Get EFIComponents from e.g. usr/lib/efi.
///
/// Each component lives at `<usr_path>/<name>/<version>/`.  When the version
/// directory contains an `EFI/` subdirectory the content is EFI-specific
/// (shim, grub, etc.) and gets the `EFI/` prefix on the ESP.  Otherwise the
/// files are copied directly to the root of the ESP (e.g. RPi firmware).
fn get_efi_component_from_usr<'a>(
    sysroot: &'a Utf8Path,
    usr_path: &'a str,
) -> Result<Option<Vec<EFIComponent>>> {
    let efilib_path = sysroot.join(usr_path);
    let skip_count = Utf8Path::new(usr_path).components().count();

    let mut components: Vec<EFIComponent> = Vec::new();

    for entry in WalkDir::new(&efilib_path)
        .min_depth(2)
        .max_depth(2)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        if !entry.file_type().is_dir() {
            continue;
        }

        let abs_path = entry.path();
        let rel_path = match abs_path.strip_prefix(sysroot) {
            Ok(p) => p,
            Err(_) => continue,
        };
        let utf8_rel_path = match Utf8PathBuf::from_path_buf(rel_path.to_path_buf()) {
            Ok(p) => p,
            Err(_) => continue,
        };

        let mut comps = utf8_rel_path.components();
        let Some(name) = comps.nth(skip_count).map(|c| c.to_string()) else {
            continue;
        };
        let Some(version) = comps.next().map(|c| c.to_string()) else {
            continue;
        };

        let efi_subdir = abs_path.join("EFI");
        if efi_subdir.exists() && efi_subdir.is_dir() {
            components.push(EFIComponent {
                name,
                version,
                path: utf8_rel_path.join("EFI"),
                has_efi_subdir: true,
            });
        } else {
            let has_content = WalkDir::new(abs_path)
                .min_depth(1)
                .into_iter()
                .filter_map(|e| e.ok())
                .any(|e| e.file_type().is_file());
            if has_content {
                components.push(EFIComponent {
                    name,
                    version,
                    path: utf8_rel_path,
                    has_efi_subdir: false,
                });
            }
        }
    }

    if components.is_empty() {
        return Ok(None);
    }
    components.sort_by(|a, b| a.name.cmp(&b.name));

    Ok(Some(components))
}

/// Given a list of EFI components (potentially with multiple versions per
/// component name), return only the latest version for each name.
///
/// Versions are compared lexicographically; this is sufficient for RPM EVR
/// strings where the epoch:version-release ordering is consistent.
fn latest_versions(components: &[EFIComponent]) -> Vec<&EFIComponent> {
    let mut by_name: std::collections::HashMap<&str, &EFIComponent> =
        std::collections::HashMap::new();
    for c in components {
        let entry = by_name.entry(c.name.as_str()).or_insert(c);
        if Version::from(c.version.as_str()) > Version::from(entry.version.as_str()) {
            *entry = c;
        }
    }
    let mut result: Vec<_> = by_name.into_values().collect();
    result.sort_by(|a, b| a.name.cmp(&b.name));
    result
}

/// Copy files from usr/lib/ostree-boot/efi/ to usr/lib/efi/<component>/<evr>/
///
/// Walks the entire `efi/` directory (both `EFI/` subdirectories and
/// root-level firmware files) and uses `rpm -qf` to determine which
/// package owns each file so it can be placed in the right component
/// directory under `usr/lib/efi/`.
fn transfer_ostree_boot_to_usr(sysroot: &Path) -> Result<()> {
    transfer_ostree_boot_to_usr_impl(sysroot, |sysroot_path, filepath| {
        let boot_filepath = Path::new("/boot/efi").join(filepath);
        crate::packagesystem::query_file(
            sysroot_path.to_str().unwrap(),
            boot_filepath.to_str().unwrap(),
        )
    })
}

/// Inner implementation that accepts a package-resolver callback so it
/// can be unit-tested without a real RPM database.
///
/// `resolve_pkg(sysroot, filepath)` must return `"<name> <evr>"`.
fn transfer_ostree_boot_to_usr_impl<F>(sysroot: &Path, resolve_pkg: F) -> Result<()>
where
    F: Fn(&Path, &Path) -> Result<String>,
{
    let ostreeboot_efi = Path::new(ostreeutil::BOOT_PREFIX).join("efi");
    let ostreeboot_efi_path = sysroot.join(&ostreeboot_efi);

    if !ostreeboot_efi_path.exists() {
        return Ok(());
    }

    let sysroot_dir = openat::Dir::open(sysroot)?;
    // Source dir is usr/lib/ostree-boot/efi
    let src = sysroot_dir
        .sub_dir(&ostreeboot_efi)
        .context("Opening ostree-boot/efi dir")?;

    for entry in WalkDir::new(&ostreeboot_efi_path) {
        let entry = entry?;
        if !entry.file_type().is_file() {
            continue;
        }

        // get path relative to the efi/ root (e.g. EFI/BOOT/shim.efi or start4.elf)
        let filepath = entry.path().strip_prefix(&ostreeboot_efi_path)?;

        // Run `rpm -qf /boot/efi/<filepath>` to find the owning package
        let pkg = resolve_pkg(sysroot, filepath)?;

        let (name, evr) = pkg
            .split_once(' ')
            .with_context(|| format!("parsing rpm output: {}", pkg))?;
        // get path usr/lib/efi/<component>/<evr>
        let efilib_path = Path::new(EFILIB).join(name).join(evr);

        // Ensure dest parent directory exists
        if let Some(parent) = efilib_path.join(filepath).parent() {
            sysroot_dir.ensure_dir_all(parent, 0o755)?;
        }

        // Dest dir is usr/lib/efi/<component>/<evr>
        let dest = sysroot_dir
            .sub_dir(&efilib_path)
            .context("Opening usr/lib/efi dir")?;
        // Copy file from ostree-boot to usr/lib/efi
        src.copy_file_at(filepath, &dest, filepath)
            .context("Copying file to usr/lib/efi")?;
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
                    has_efi_subdir: true,
                },
                EFIComponent {
                    name: "FOO".to_string(),
                    version: "1.1".to_string(),
                    path: Utf8PathBuf::from("usr/lib/efi/FOO/1.1/EFI"),
                    has_efi_subdir: true,
                },
            ])
        );
        std::fs::remove_dir_all(efi_path.join("BAR/1.1/EFI"))?;
        std::fs::remove_dir_all(efi_path.join("FOO/1.1/EFI"))?;
        let efi_comps = get_efi_component_from_usr(utf8_tpath, EFILIB)?;
        assert_eq!(efi_comps, None);
        Ok(())
    }

    #[test]
    fn test_get_efi_component_mixed() -> Result<()> {
        let tmpdir = tempfile::tempdir()?;
        let tpath = tmpdir.path();
        let efi_path = tpath.join("usr/lib/efi");

        // EFI component (has EFI/ subdirectory)
        let efi_comp_dir = efi_path.join("FOO/1.0/EFI/vendor");
        std::fs::create_dir_all(&efi_comp_dir)?;
        std::fs::write(efi_comp_dir.join("foo.efi"), "foo data")?;

        // Non-EFI component (files directly in version dir)
        let non_efi_dir = efi_path.join("BAR/2.0");
        std::fs::create_dir_all(&non_efi_dir)?;
        std::fs::write(non_efi_dir.join("bar.dtb"), "bar data")?;
        std::fs::write(non_efi_dir.join("baz.bin"), "baz data")?;

        // Empty version dir (should be ignored)
        std::fs::create_dir_all(efi_path.join("EMPTY/1.0"))?;

        let utf8_tpath =
            Utf8Path::from_path(tpath).ok_or_else(|| anyhow::anyhow!("Path is not valid UTF-8"))?;
        let comps = get_efi_component_from_usr(utf8_tpath, EFILIB)?;
        let comps = comps.expect("components should be found");

        assert_eq!(comps.len(), 2);
        assert_eq!(comps[0].name, "BAR");
        assert!(!comps[0].has_efi_subdir);
        assert_eq!(comps[0].path, Utf8PathBuf::from("usr/lib/efi/BAR/2.0"));

        assert_eq!(comps[1].name, "FOO");
        assert!(comps[1].has_efi_subdir);
        assert_eq!(comps[1].path, Utf8PathBuf::from("usr/lib/efi/FOO/1.0/EFI"));
        Ok(())
    }

    #[test]
    fn test_new_from_dir_strip_prefix() -> Result<()> {
        let tmpdir = tempfile::tempdir()?;
        let efilib = tmpdir.path().join("efilib");

        // EFI component: FOO/1.0/EFI/vendor/foo.efi
        std::fs::create_dir_all(efilib.join("FOO/1.0/EFI/vendor"))?;
        std::fs::write(efilib.join("FOO/1.0/EFI/vendor/foo.efi"), "foo data")?;

        // Non-EFI component: BAR/2.0/{bar.dtb,baz.bin}
        std::fs::create_dir_all(efilib.join("BAR/2.0"))?;
        std::fs::write(efilib.join("BAR/2.0/bar.dtb"), "bar data")?;
        std::fs::write(efilib.join("BAR/2.0/baz.bin"), "baz data")?;

        let dir = openat::Dir::open(&efilib)?;
        let ft = filetree::FileTree::new_from_dir_strip_prefix_for(&dir, &["FOO/1.0", "BAR/2.0"])?;

        assert!(ft.children.contains_key("EFI/vendor/foo.efi"));
        assert!(ft.children.contains_key("bar.dtb"));
        assert!(ft.children.contains_key("baz.bin"));

        // Source paths should point back to the original relative paths
        assert_eq!(
            ft.children["EFI/vendor/foo.efi"].source.as_deref(),
            Some("FOO/1.0/EFI/vendor/foo.efi")
        );
        assert_eq!(
            ft.children["bar.dtb"].source.as_deref(),
            Some("BAR/2.0/bar.dtb")
        );

        // ensure_efi_prefix should NOT re-prefix (some keys already have EFI/)
        let efi = Efi::default();
        let ft2 = efi.ensure_efi_prefix(ft.clone());
        assert_eq!(
            ft.children.keys().collect::<Vec<_>>(),
            ft2.children.keys().collect::<Vec<_>>()
        );

        Ok(())
    }

    #[test]
    fn test_rpi4_esp_update_flow() -> Result<()> {
        let tmpdir = tempfile::tempdir()?;
        let p = tmpdir.path();

        // Source directory simulating EFILIB with mixed EFI and non-EFI
        // components (e.g. bootloader + root-level firmware)
        let src = p.join("src");
        std::fs::create_dir_all(src.join("FOO/1.0/EFI/vendor"))?;
        std::fs::write(src.join("FOO/1.0/EFI/vendor/foo.efi"), "foo data")?;
        let fw_dir = src.join("BAR/2.0");
        std::fs::create_dir_all(&fw_dir)?;
        std::fs::write(fw_dir.join("bar.dtb"), "bar data")?;
        std::fs::write(fw_dir.join("baz.bin"), "baz data")?;
        std::fs::write(fw_dir.join("quux.dat"), "quux data")?;
        std::fs::write(fw_dir.join("conf.txt"), "conf data")?;

        let dst = p.join("dst");
        std::fs::create_dir_all(&dst)?;

        let src_dir = openat::Dir::open(&src)?;
        let dst_dir = openat::Dir::open(&dst)?;

        let ft =
            filetree::FileTree::new_from_dir_strip_prefix_for(&src_dir, &["FOO/1.0", "BAR/2.0"])?;

        assert!(ft.children.contains_key("EFI/vendor/foo.efi"));
        assert!(ft.children.contains_key("bar.dtb"));
        assert!(ft.children.contains_key("baz.bin"));
        assert!(ft.children.contains_key("quux.dat"));
        assert!(ft.children.contains_key("conf.txt"));

        // Install to empty ESP
        let empty_ft = filetree::FileTree {
            children: std::collections::BTreeMap::new(),
        };
        let diff = empty_ft.diff(&ft)?;
        assert_eq!(diff.additions.len(), 5);

        filetree::apply_diff(&src_dir, &dst_dir, &diff, None)?;

        assert!(dst_dir.exists("EFI/vendor/foo.efi")?);
        assert!(dst_dir.exists("bar.dtb")?);
        assert!(dst_dir.exists("baz.bin")?);
        assert!(dst_dir.exists("quux.dat")?);
        assert!(dst_dir.exists("conf.txt")?);
        assert_eq!(
            std::fs::read_to_string(dst.join("EFI/vendor/foo.efi"))?,
            "foo data"
        );
        assert_eq!(std::fs::read_to_string(dst.join("bar.dtb"))?, "bar data");

        // Simulate update (change one non-EFI file)
        std::fs::write(fw_dir.join("bar.dtb"), "bar data v2")?;
        let ft_v2 =
            filetree::FileTree::new_from_dir_strip_prefix_for(&src_dir, &["FOO/1.0", "BAR/2.0"])?;

        let diff2 = ft.diff(&ft_v2)?;
        assert_eq!(diff2.changes.len(), 1);
        assert!(diff2.changes.contains("bar.dtb"));

        filetree::apply_diff(&src_dir, &dst_dir, &diff2, None)?;
        assert_eq!(std::fs::read_to_string(dst.join("bar.dtb"))?, "bar data v2");
        assert_eq!(
            std::fs::read_to_string(dst.join("EFI/vendor/foo.efi"))?,
            "foo data"
        );

        Ok(())
    }

    #[test]
    fn test_transfer_ostree_boot_to_usr() -> Result<()> {
        let tmpdir = tempfile::tempdir()?;
        let sysroot = tmpdir.path();

        // Simulate usr/lib/ostree-boot/efi/ with both EFI/ and root-level files
        let efi_dir = sysroot.join("usr/lib/ostree-boot/efi");
        std::fs::create_dir_all(efi_dir.join("EFI/vendor"))?;
        std::fs::write(efi_dir.join("EFI/vendor/foo.efi"), "foo data")?;
        std::fs::create_dir_all(efi_dir.join("EFI/BOOT"))?;
        std::fs::write(efi_dir.join("EFI/BOOT/BOOTAA64.EFI"), "boot data")?;
        // Root-level files
        std::fs::write(efi_dir.join("bar.dtb"), "bar data")?;
        std::fs::write(efi_dir.join("baz.bin"), "baz data")?;
        std::fs::create_dir_all(efi_dir.join("sub"))?;
        std::fs::write(efi_dir.join("sub/quux.dat"), "quux data")?;

        // Ensure the destination base directory exists
        std::fs::create_dir_all(sysroot.join(EFILIB))?;

        // Fake resolver: EFI files belong to "FOO 1.0", root-level
        // files to "BAR 2.0"
        let resolve = |_sysroot: &Path, filepath: &Path| -> Result<String> {
            let s = filepath.to_str().unwrap();
            if s.starts_with("EFI") {
                Ok("FOO 1.0".to_string())
            } else {
                Ok("BAR 2.0".to_string())
            }
        };

        transfer_ostree_boot_to_usr_impl(sysroot, resolve)?;

        // EFI files should be under EFILIB/FOO/1.0/EFI/...
        let foo_base = sysroot.join("usr/lib/efi/FOO/1.0");
        assert_eq!(
            std::fs::read_to_string(foo_base.join("EFI/vendor/foo.efi"))?,
            "foo data"
        );
        assert_eq!(
            std::fs::read_to_string(foo_base.join("EFI/BOOT/BOOTAA64.EFI"))?,
            "boot data"
        );

        // Root-level files should be under EFILIB/BAR/2.0/
        let bar_base = sysroot.join("usr/lib/efi/BAR/2.0");
        assert_eq!(
            std::fs::read_to_string(bar_base.join("bar.dtb"))?,
            "bar data"
        );
        assert_eq!(
            std::fs::read_to_string(bar_base.join("baz.bin"))?,
            "baz data"
        );
        assert_eq!(
            std::fs::read_to_string(bar_base.join("sub/quux.dat"))?,
            "quux data"
        );

        Ok(())
    }
}
