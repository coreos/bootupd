use anyhow::{bail, Context, Result};
use camino::Utf8PathBuf;
use cap_std::ambient_authority;
use cap_std::fs::Dir;
use cap_std_ext::dirext::CapStdExtDirExt;
use fn_error_context::context;
use std::collections::BTreeMap;
use std::io::prelude::*;
use std::path::{Path, PathBuf};
use std::process::Command;

use bootc_internal_blockdev::Device;

use crate::bootloader::Bootloader;
use crate::bootupd::{RootContext, Sysroot};
use crate::component::*;
use crate::filetree::FileMetadata;
use crate::filetree::FileTree;
use crate::freezethaw::fsfreeze_thaw_cycle;
use crate::grubconfigs;
use crate::model::*;
use crate::packagesystem;

// TODO: grub bin and modules should be handled differently when we're making this
//       distro-agnostic. See https://github.com/coreos/bootupd/issues/810

// grub2-install file path
pub(crate) const GRUB_BIN: &str = "usr/sbin/grub2-install";

/// The GRUB target directory holding the images and modules which
/// grub2-install embeds into the boot device
#[cfg(target_arch = "x86_64")]
const GRUB_MODULES: &str = "usr/lib/grub/i386-pc";
#[cfg(target_arch = "powerpc64")]
const GRUB_MODULES: &str = "usr/lib/grub/powerpc-ieee1275";

#[cfg(target_arch = "powerpc64")]
fn target_device(device: &Device) -> Result<String> {
    const PREPBOOT_GUID: &str = "9E1A2D38-C612-4316-AA26-8B49521E5A8B";
    /// We make a best-effort to support MBR partitioning too.
    const PREPBOOT_MBR_TYPE: &str = "41";

    if !device.has_children() {
        return Ok(device.path());
    }

    let prepdev = device
        .find_partition_of_type(PREPBOOT_GUID)
        .or_else(|| device.find_partition_of_type(PREPBOOT_MBR_TYPE))
        .ok_or_else(|| {
            anyhow::anyhow!("Failed to find PReP partition with GUID {PREPBOOT_GUID}")
        })?;
    Ok(prepdev.path())
}

/// Hash every file under the GRUB target directory,
/// keyed by `<GRUB_MODULES>/<relative path>`
#[context("Hashing GRUB files in directory at {path}")]
fn hash_grub_dir(
    dir: &Dir,
    path: &str,
    children: &mut BTreeMap<String, FileMetadata>,
) -> Result<()> {
    for entry in dir.entries_utf8()? {
        let entry = entry?;
        let name = entry.file_name().context("getting file name")?;
        let rel_path = format!("{path}/{name}");
        let file_type = entry.file_type()?;
        if file_type.is_file() {
            let meta = FileMetadata::new_from_path(dir, &name)
                .with_context(|| format!("hashing {rel_path}"))?;
            children.insert(rel_path, meta);
        } else if file_type.is_dir() {
            let child = dir.open_dir(&name)?;
            hash_grub_dir(&child, &rel_path, children)?;
        } else {
            bail!("Unsupported file type in the GRUB modules directory: {rel_path}");
        }
    }
    Ok(())
}

/// The BIOS update payload is the GRUB installer binary plus its inputs.
///
/// The images and modules can change without the installer binary changing,
/// and are embedded into the installed boot code. A sysroot without
/// the modules is fingerprinted by the installer binary alone.
#[context("Generating GRUB FileTree from root {}", root.as_ref().display())]
fn grub_filetree(root: impl AsRef<Path>) -> Result<FileTree> {
    let root = root.as_ref();
    let dir = Dir::open_ambient_dir(root, ambient_authority())
        .with_context(|| format!("opening {}", root.display()))?;

    let mut children = BTreeMap::new();
    let meta = FileMetadata::new_from_path(&dir, GRUB_BIN)
        .with_context(|| format!("hashing {GRUB_BIN}"))?;
    children.insert(GRUB_BIN.into(), meta);

    if let Some(grubdir) = dir.open_dir_optional(GRUB_MODULES)? {
        hash_grub_dir(&grubdir, GRUB_MODULES, &mut children)?;
    }

    Ok(FileTree { children })
}

#[derive(Default)]
pub(crate) struct Bios {}

impl Bios {
    // Return `true` if grub2-modules installed
    fn check_grub_modules(&self) -> Result<bool> {
        Path::new("/")
            .join(GRUB_MODULES)
            .try_exists()
            .context("checking for GRUB modules dir")
    }

    // Run grub2-install from the process root, returning that root so
    // callers can identify the installer which was actually used
    fn run_grub_install(&self, dest_root: &str, device: &Device) -> Result<PathBuf> {
        if !self.check_grub_modules()? {
            bail!("Failed to find grub2-modules");
        }
        let process_root = PathBuf::from("/");
        let grub_install = process_root.join(GRUB_BIN);
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
            .arg(&device.path());

        #[cfg(target_arch = "powerpc64")]
        {
            let device = target_device(device)?;
            cmd.args(&["--target", "powerpc-ieee1275"])
                .args(&["--boot-directory", boot_dir.to_str().unwrap()])
                .arg("--no-nvram")
                .arg(&device);
        }

        let cmdout = cmd.output()?;
        if !cmdout.status.success() {
            std::io::stderr().write_all(&cmdout.stderr)?;
            bail!("Failed to run {:?}", cmd);
        }
        Ok(process_root)
    }
}

impl Component for Bios {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn name(&self) -> &'static str {
        self.component_type().into()
    }

    fn component_type(&self) -> ComponentType {
        ComponentType::Bios
    }

    fn is_bootloader_supported(&self, bootloader: Bootloader) -> bool {
        matches!(bootloader, Bootloader::Grub)
    }

    fn install(
        &self,
        src_root: &str,
        dest_root: &str,
        device: Option<&Device>,
        _update_firmware: bool,
        bootloader: Bootloader,
    ) -> Result<InstalledContent> {
        if !self.is_bootloader_supported(bootloader) {
            anyhow::bail!("{bootloader} cannot be installed for {}", self.name());
        }

        let device =
            device.ok_or_else(|| anyhow::anyhow!("BIOS component requires a target device"))?;
        let src_dir = Dir::open_ambient_dir(src_root, ambient_authority())
            .with_context(|| format!("opening source directory {src_root}"))?;
        let Some(meta) = self.get_component_update(&src_dir, Some(bootloader))? else {
            anyhow::bail!("No update metadata for component {} found", self.name());
        };

        // Record the installer which was actually used, which comes from
        // the process root rather than from src_root (when they are not equivalent)
        let root = self
            .run_grub_install(dest_root, device)
            .with_context(|| format!("installing GRUB on {}", device.path()))?;

        Ok(InstalledContent {
            meta,
            filetree: Some(grub_filetree(&root)?),
            adopted_from: None,
        })
    }

    fn generate_update_metadata(&self, sysroot_path: &str) -> Result<Option<ContentMetadata>> {
        let grub_install = Path::new(sysroot_path).join(GRUB_BIN);
        if !grub_install.exists() {
            println!("Failed to find {:?}", grub_install);
            return Ok(None);
        }

        // Query the rpm database and list the package and build times for /usr/sbin/grub2-install
        let meta = packagesystem::query_files(sysroot_path, [&grub_install])?;
        write_update_metadata(sysroot_path, self.component_update_data_name(), &meta)?;
        Ok(Some(meta))
    }

    fn query_adopt(&self, devices: &Option<Vec<Device>>) -> Result<Option<Adoptable>> {
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
    fn migrate_static_grub_config(&self, sysroot_path: &str, destdir: &Dir) -> Result<()> {
        let grub = "boot/grub2";
        // sysroot_path is /, destdir is Dir of /
        let grub_config_path = Utf8PathBuf::from(sysroot_path).join(grub);
        let grub_config_dir = destdir.open_dir(grub).context("Opening boot/grub2")?;

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

        crate::grubconfigs::install(&destdir, None, None, true, None)?;

        // Remove the real config if it is symlink and will not
        // if /boot/grub2/grub.cfg is file
        if current_config != grub_config {
            println!("Removing {}", current_config);

            // current_config is an absolute path of the form
            //
            // /boot/grub2/../loader/grub.cfg
            //
            // as it's a RELATIVE symlink from /boot/grub2/grub.cfg -> /boot/loader/grub.cfg
            match std::fs::remove_file(current_config.as_std_path()) {
                Ok(..) => { /* No op */ }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    println!("{} not found for removal", current_config);
                }
                Err(e) => Err(e).with_context(|| format!("Removing {current_config}"))?,
            };
        }

        // Synchronize the filesystem containing /boot/grub2 to disk.
        fsfreeze_thaw_cycle(grub_config_dir.reopen_as_ownedfd()?)?;

        Ok(())
    }

    fn adopt_update(
        &self,
        rootcxt: &RootContext,
        update: &ContentMetadata,
        with_static_config: bool,
        bootloader: Bootloader,
    ) -> Result<Option<InstalledContent>> {
        if !self.is_bootloader_supported(bootloader) {
            log::debug!(
                "Skip BIOS adopt: bootloader {} is not supported",
                bootloader.efi_component_name()
            );
            return Ok(None);
        }

        let bios_devices = rootcxt.device.find_colocated_bios_boot()?;
        let Some(meta) = self.query_adopt(&bios_devices)? else {
            return Ok(None);
        };

        // Install grub onto each parent (whole-disk) device. For BIOS boot on
        // multi-disk setups (e.g. RAID), grub must be written to every backing
        // disk's MBR/BIOS-boot partition so the system can boot from any one.
        for parent in rootcxt.device.find_all_roots()? {
            self.run_grub_install(rootcxt.path.as_str(), &parent)?;
            log::debug!("Installed grub modules on {}", parent.path());
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
            filetree: Some(grub_filetree(&rootcxt.path)?),
            adopted_from: Some(meta.version),
        }))
    }

    fn query_update(
        &self,
        sysroot: &Dir,
        bootloader: Bootloader,
    ) -> Result<Option<ContentMetadata>> {
        if !self.is_bootloader_supported(bootloader) {
            return Ok(None);
        }

        self.get_component_update(sysroot, Some(bootloader))
    }

    fn query_requires_update(&self, sysroot: &Dir) -> Result<()> {
        // Failed as expected if booted with BIOS and no update metadata
        if !sysroot.exists("sys/firmware/efi") {
            anyhow::bail!("Failed to find BIOS update metadata");
        }
        Ok(())
    }

    fn run_update(&self, rootcxt: &RootContext, _: &InstalledContent) -> Result<InstalledContent> {
        let updatemeta = self
            .query_update(&rootcxt.sysroot, Bootloader::Grub)?
            .expect("update available");

        for parent in rootcxt.device.find_all_roots()? {
            self.run_grub_install(rootcxt.path.as_str(), &parent)?;
            log::debug!("Installed grub modules on {}", parent.path());
        }

        let adopted_from = None;
        Ok(InstalledContent {
            meta: updatemeta,
            filetree: Some(grub_filetree(&rootcxt.path)?),
            adopted_from,
        })
    }

    fn validate(&self, _: &InstalledContent, _device: &Device) -> Result<ValidationResult> {
        Ok(ValidationResult::Skip)
    }

    fn get_efi_vendor(&self, _: &Path) -> Result<Option<String>> {
        Ok(None)
    }

    fn has_changed_content(
        &self,
        sysroot: Sysroot<'_>,
        current: &InstalledContent,
        _bootloader: Bootloader,
    ) -> Result<bool> {
        let updated = grub_filetree(sysroot.path)?;
        if let Some(t) = current.filetree.as_ref() {
            Ok(!t.is_content_eq(&updated))
        } else {
            // Fallback path for existing installs which don't store a filetree for the BIOS component
            log::warn!("no filetree available for the current install, assuming updatable");
            Ok(true)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::packagesystem::Module;
    use camino::Utf8Path;
    use chrono::Utc;
    use std::path::PathBuf;

    #[test]
    fn test_query_updatable_content_differs() -> Result<()> {
        let sysroot = tempfile::tempdir()?;
        let sysroot_path = Utf8Path::from_path(sysroot.path()).expect("valid UTF-8");

        // Create update payload
        std::fs::create_dir_all(sysroot_path.join("usr/sbin"))?;
        std::fs::write(sysroot_path.join(GRUB_BIN), "grub2-install new")?;
        std::fs::create_dir_all(sysroot_path.join(BOOTUPD_UPDATES_DIR))?;
        let meta = ContentMetadata {
            timestamp: Utc::now(),
            version: "grub2 2.12-7.el9".into(),
            versions: Some(vec![Module {
                name: "grub2".into(),
                evr: "2.12-7.el9".into(),
            }]),
            #[cfg(efi_arch)]
            default_bootloader: None,
        };
        write_update_metadata(sysroot_path.as_str(), PathBuf::from("BIOS.json"), &meta)?;

        // A payload with the content that is currently installed
        let old_payload = tempfile::tempdir()?;
        std::fs::create_dir_all(old_payload.path().join("usr/sbin"))?;
        std::fs::write(old_payload.path().join(GRUB_BIN), "grub2-install old")?;

        let sysroot_dir = Dir::open_ambient_dir(sysroot_path, ambient_authority())?;
        let sysroot = Sysroot {
            dir: &sysroot_dir,
            path: sysroot_path,
        };
        let bios = Bios::default();

        // Changed content wins even though the metadata is identical
        let inst = InstalledContent {
            meta: meta.clone(),
            filetree: Some(grub_filetree(old_payload.path())?),
            adopted_from: None,
        };
        let (update, updatable) = bios.query_updatable(sysroot, &inst, Bootloader::Grub)?;
        assert!(update.is_some());
        assert!(matches!(updatable, ComponentUpdatable::Upgradable));

        // Installs from before filetrees were stored, update once
        let inst = InstalledContent {
            meta: meta.clone(),
            filetree: None,
            adopted_from: None,
        };
        let (_, updatable) = bios.query_updatable(sysroot, &inst, Bootloader::Grub)?;
        assert!(matches!(updatable, ComponentUpdatable::Upgradable));

        // Identical content, no update
        let inst = InstalledContent {
            meta: meta.clone(),
            filetree: Some(grub_filetree(sysroot.path)?),
            adopted_from: None,
        };
        let (_, updatable) = bios.query_updatable(sysroot, &inst, Bootloader::Grub)?;
        assert!(matches!(updatable, ComponentUpdatable::AtLatestVersion));

        Ok(())
    }

    #[test]
    fn test_grub_filetree_includes_modules() -> Result<()> {
        let payload = tempfile::tempdir()?;
        std::fs::create_dir_all(payload.path().join("usr/sbin"))?;
        std::fs::write(payload.path().join(GRUB_BIN), "grub2-install")?;
        let modules = payload.path().join(GRUB_MODULES);
        std::fs::create_dir_all(&modules)?;
        std::fs::write(modules.join("kernel.img"), "kernel")?;
        let fonts = modules.join("fonts");
        std::fs::create_dir_all(&fonts)?;
        std::fs::write(fonts.join("unicode.pf2"), "font")?;

        let tree = grub_filetree(payload.path())?;
        assert!(tree.children.contains_key(GRUB_BIN));
        assert!(tree
            .children
            .contains_key(&format!("{GRUB_MODULES}/kernel.img")));

        // The fingerprint changes when a module changes but grub2-install
        // does not
        std::fs::write(
            payload.path().join(GRUB_MODULES).join("kernel.img"),
            "kernel new",
        )?;
        let changed = grub_filetree(payload.path())?;
        assert!(!tree.is_content_eq(&changed));

        // Nested file is keyed by its full path under the modules dir
        let tree = grub_filetree(payload.path())?;
        assert!(tree
            .children
            .contains_key(&format!("{GRUB_MODULES}/fonts/unicode.pf2")));

        // A change in the nested dir changes the tree
        std::fs::write(fonts.join("unicode.pf2"), "font new")?;
        let changed = grub_filetree(payload.path())?;
        assert!(!tree.is_content_eq(&changed));

        Ok(())
    }

    #[test]
    fn test_grub_filetree_refuses_symlinks_in_modules() -> Result<()> {
        let payload = tempfile::tempdir()?;
        std::fs::create_dir_all(payload.path().join("usr/sbin"))?;
        std::fs::write(payload.path().join(GRUB_BIN), "grub2-install")?;
        let modules = payload.path().join(GRUB_MODULES);
        std::fs::create_dir_all(&modules)?;
        std::os::unix::fs::symlink("target", modules.join("link"))?;

        let err = grub_filetree(payload.path())
            .expect_err("a symlink in the modules directory should be refused");
        assert!(format!("{err:#}").contains("Unsupported file type"));
        Ok(())
    }
}
