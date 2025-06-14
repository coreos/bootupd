use anyhow::{bail, Context, Result};
use camino::Utf8PathBuf;
use openat_ext::OpenatDirExt;
#[cfg(target_arch = "powerpc64")]
use std::borrow::Cow;
use std::io::prelude::*;
use std::path::Path;
use std::process::Command;

use crate::blockdev;
use crate::bootupd::RootContext;
use crate::component::*;
use crate::grubconfigs;
use crate::model::*;
use crate::packagesystem;

// grub2-install file path
pub(crate) const GRUB_BIN: &str = "usr/sbin/grub2-install";

#[cfg(target_arch = "powerpc64")]
fn target_device(device: &str) -> Result<Cow<str>> {
    const PREPBOOT_GUID: &str = "9E1A2D38-C612-4316-AA26-8B49521E5A8B";
    /// We make a best-effort to support MBR partitioning too.
    const PREPBOOT_MBR_TYPE: &str = "41";

    // Here we use lsblk to see if the device has any partitions at all
    let dev = bootc_blockdev::list_dev(device.into())?;
    if dev.children.is_none() {
        return Ok(device.into());
    };
    // If it does, directly call `sfdisk` and bypass lsblk because inside a container
    // we may not have all the cached udev state (that I think is in /run).
    let device = bootc_blockdev::partitions_of(device.into())?;
    let prepdev = device
        .partitions
        .iter()
        .find(|p| matches!(p.parttype.as_str(), PREPBOOT_GUID | PREPBOOT_MBR_TYPE))
        .ok_or_else(|| {
            anyhow::anyhow!("Failed to find PReP partition with GUID {PREPBOOT_GUID}")
        })?;
    Ok(prepdev.path().as_str().to_owned().into())
}

#[derive(Default)]
pub(crate) struct Bios {}

impl Bios {
    // Return `true` if grub2-modules installed
    fn check_grub_modules(&self) -> Result<bool> {
        let usr_path = Path::new("/usr/lib/grub");
        #[cfg(target_arch = "x86_64")]
        {
            usr_path.join("i386-pc").try_exists().map_err(Into::into)
        }
        #[cfg(target_arch = "powerpc64")]
        {
            usr_path
                .join("powerpc-ieee1275")
                .try_exists()
                .map_err(Into::into)
        }
    }

    // Run grub2-install
    fn run_grub_install(&self, dest_root: &str, device: &str) -> Result<()> {
        if !self.check_grub_modules()? {
            bail!("Failed to find grub2-modules");
        }
        let grub_install = Path::new("/").join(GRUB_BIN);
        if !grub_install.exists() {
            bail!("Failed to find {:?}", grub_install);
        }

        let mut cmd = Command::new(grub_install);
        let boot_dir = Path::new(dest_root).join("boot");
        // We forcibly inject mdraid1x because it's needed by CoreOS's default of "install raw disk image"
        // We also add part_gpt because in some cases probing of the partition map can fail such
        // as in a container, but we always use GPT.
        #[cfg(target_arch = "x86_64")]
        cmd.args(["--target", "i386-pc"])
            .args(["--boot-directory", boot_dir.to_str().unwrap()])
            .args(["--modules", "mdraid1x part_gpt"])
            .arg(device);

        #[cfg(target_arch = "powerpc64")]
        {
            let device = target_device(device)?;
            cmd.args(&["--target", "powerpc-ieee1275"])
                .args(&["--boot-directory", boot_dir.to_str().unwrap()])
                .arg("--no-nvram")
                .arg(&*device);
        }

        let cmdout = cmd.output()?;
        if !cmdout.status.success() {
            std::io::stderr().write_all(&cmdout.stderr)?;
            bail!("Failed to run {:?}", cmd);
        }
        Ok(())
    }
}

impl Component for Bios {
    fn name(&self) -> &'static str {
        "BIOS"
    }

    fn install(
        &self,
        src_root: &openat::Dir,
        dest_root: &str,
        device: &str,
        _update_firmware: bool,
    ) -> Result<InstalledContent> {
        let Some(meta) = get_component_update(src_root, self)? else {
            anyhow::bail!("No update metadata for component {} found", self.name());
        };

        self.run_grub_install(dest_root, device)?;
        Ok(InstalledContent {
            meta,
            filetree: None,
            adopted_from: None,
        })
    }

    fn generate_update_metadata(&self, sysroot_path: &str) -> Result<ContentMetadata> {
        let grub_install = Path::new(sysroot_path).join(GRUB_BIN);
        if !grub_install.exists() {
            bail!("Failed to find {:?}", grub_install);
        }

        // Query the rpm database and list the package and build times for /usr/sbin/grub2-install
        let meta = packagesystem::query_files(sysroot_path, [&grub_install])?;
        write_update_metadata(sysroot_path, self, &meta)?;
        Ok(meta)
    }

    fn query_adopt(&self, devices: &Option<Vec<String>>) -> Result<Option<Adoptable>> {
        #[cfg(target_arch = "x86_64")]
        if crate::efi::is_efi_booted()? && devices.is_none() {
            log::debug!("Skip BIOS adopt");
            return Ok(None);
        }
        crate::component::query_adopt_state()
    }

    // Backup the current grub.cfg and replace with new static config
    // - Backup "/boot/loader/grub.cfg" to "/boot/grub2/grub.cfg.bak"
    // - Remove symlink "/boot/grub2/grub.cfg"
    // - Replace "/boot/grub2/grub.cfg" symlink with new static "grub.cfg"
    fn migrate_static_grub_config(&self, sysroot_path: &str, destdir: &openat::Dir) -> Result<()> {
        let grub = "boot/grub2";
        // sysroot_path is /, destdir is Dir of /
        let grub_config_path = Utf8PathBuf::from(sysroot_path).join(grub);
        let grub_config_dir = destdir.sub_dir(grub).context("Opening boot/grub2")?;

        let grub_config = grub_config_path.join(grubconfigs::GRUBCONFIG);

        if !grub_config.exists() {
            anyhow::bail!("Could not find '{}'", grub_config);
        }

        let mut current_config;
        // If /boot/grub2/grub.cfg is not symlink, we need to keep going
        if !grub_config.is_symlink() {
            println!("'{}' is not a symlink", grub_config);
            current_config = grub_config.clone();
        } else {
            // If /boot/grub2/grub.cfg is symlink to /boot/loader/grub.cfg,
            // backup it to /boot/grub2/grub.cfg.bak
            // Get real file for symlink /boot/grub2/grub.cfg
            let real_config = grub_config_dir.read_link(grubconfigs::GRUBCONFIG)?;
            let real_config =
                Utf8PathBuf::from_path_buf(real_config).expect("Path should be valid UTF-8");
            // Resolve symlink location
            current_config = grub_config_path.clone();
            current_config.push(real_config);
        }

        let backup_config = grub_config_path.join(grubconfigs::GRUBCONFIG_BACKUP);
        if !backup_config.exists() {
            // Backup the current GRUB config which is hopefully working right now
            println!(
                "Creating a backup of the current GRUB config '{}' in '{}'...",
                current_config, backup_config
            );
            std::fs::copy(&current_config, &backup_config)
                .context("Failed to backup GRUB config")?;
        }

        crate::grubconfigs::install(&destdir, None, true)?;

        // Remove the real config if it is symlink and will not
        // if /boot/grub2/grub.cfg is file
        if current_config != grub_config {
            println!("Removing {}", current_config);
            grub_config_dir.remove_file_optional(current_config.as_std_path())?;
        }

        // Synchronize the filesystem containing /boot/grub2 to disk.
        let _ = grub_config_dir.syncfs();

        Ok(())
    }

    fn adopt_update(
        &self,
        rootcxt: &RootContext,
        update: &ContentMetadata,
        with_static_config: bool,
    ) -> Result<Option<InstalledContent>> {
        let bios_devices = blockdev::find_colocated_bios_boot(&rootcxt.devices)?;
        let Some(meta) = self.query_adopt(&bios_devices)? else {
            return Ok(None);
        };

        for parent in rootcxt.devices.iter() {
            self.run_grub_install(rootcxt.path.as_str(), &parent)?;
            log::debug!("Installed grub modules on {parent}");
        }

        if with_static_config {
            // Install the static config if the OSTree bootloader is not set.
            if let Some(bootloader) = crate::ostreeutil::get_ostree_bootloader()? {
                println!(
                    "ostree repo 'sysroot.bootloader' config option is currently set to: '{bootloader}'",
                );
            } else {
                println!("ostree repo 'sysroot.bootloader' config option is not set yet");
                self.migrate_static_grub_config(rootcxt.path.as_str(), &rootcxt.sysroot)?;
            };
        }
        Ok(Some(InstalledContent {
            meta: update.clone(),
            filetree: None,
            adopted_from: Some(meta.version),
        }))
    }

    fn query_update(&self, sysroot: &openat::Dir) -> Result<Option<ContentMetadata>> {
        get_component_update(sysroot, self)
    }

    fn run_update(&self, rootcxt: &RootContext, _: &InstalledContent) -> Result<InstalledContent> {
        let updatemeta = self
            .query_update(&rootcxt.sysroot)?
            .expect("update available");

        for parent in rootcxt.devices.iter() {
            self.run_grub_install(rootcxt.path.as_str(), &parent)?;
            log::debug!("Installed grub modules on {parent}");
        }

        let adopted_from = None;
        Ok(InstalledContent {
            meta: updatemeta,
            filetree: None,
            adopted_from,
        })
    }

    fn validate(&self, _: &InstalledContent) -> Result<ValidationResult> {
        Ok(ValidationResult::Skip)
    }

    fn get_efi_vendor(&self, _: &openat::Dir) -> Result<Option<String>> {
        Ok(None)
    }
}
