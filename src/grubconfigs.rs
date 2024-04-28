use std::fmt::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{anyhow, Context, Result};
use fn_error_context::context;
use openat_ext::OpenatDirExt;
use walkdir::WalkDir;

use crate::efi::SHIM;

/// The subdirectory of /boot we use
const GRUB2DIR: &str = "grub2";
const CONFIGDIR: &str = "/usr/lib/bootupd/grub2-static";
const DROPINDIR: &str = "configs.d";

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

#[context("Locating EFI vendordir")]
pub(crate) fn find_efi_vendordir(efidir: &openat::Dir, root: Option<&str>) -> Result<PathBuf> {
    let root = root.unwrap_or("/");
    let shim_img_dir = Path::new(root)
        .join(crate::model::BOOTUPD_UPDATES_DIR)
        .join("EFI");
    let shim_files = find_file_recursive(shim_img_dir, SHIM)?;

    // Does not support multiple shim in the image
    if shim_files.len() > 1 {
        anyhow::bail!("Find multiple {SHIM} in the image");
    }
    let shim_img_path = if let Some(p) = shim_files.first() {
        p
    } else {
        anyhow::bail!("Failed to find {SHIM} in the image");
    };

    // get {vendor}/shim from image
    let vendor = shim_img_path
        .parent()
        .unwrap()
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("No file name found"))?;
    let vendor_shim = format!("{}/{SHIM}", vendor.to_str().unwrap());

    let efidir = efidir.recover_path()?;
    let shim_files = find_file_recursive(&efidir.as_path(), SHIM)?;
    if shim_files.len() == 0 {
        anyhow::bail!("Failed to find {SHIM} under efi dir");
    }
    for entry in shim_files {
        // matching content
        let output = Command::new("diff")
            .arg(shim_img_path)
            .arg(&entry)
            .output()?;
        let st = output.status;
        if !st.success() {
            continue;
        }
        // matching {vendor}/shim path
        if !entry.ends_with(&vendor_shim) {
            anyhow::bail!("Match existing {SHIM} content which is not expected");
        }
        return Ok(vendor.into());
    }

    anyhow::bail!("Failed to find EFI vendor dir that matches the image")
}

/// Install the static GRUB config files.
#[context("Installing static GRUB configs")]
pub(crate) fn install(target_root: &openat::Dir, efi: bool, write_uuid: bool) -> Result<()> {
    let bootdir = &target_root.sub_dir("boot").context("Opening /boot")?;
    let boot_is_mount = {
        let root_dev = target_root.self_metadata()?.stat().st_dev;
        let boot_dev = bootdir.self_metadata()?.stat().st_dev;
        log::debug!("root_dev={root_dev} boot_dev={boot_dev}");
        root_dev != boot_dev
    };

    if !bootdir.exists(GRUB2DIR)? {
        bootdir.create_dir(GRUB2DIR, 0o700)?;
    }

    let mut config = std::fs::read_to_string(Path::new(CONFIGDIR).join("grub-static-pre.cfg"))?;

    let dropindir = openat::Dir::open(&Path::new(CONFIGDIR).join(DROPINDIR))?;
    // Sort the files for reproducibility
    let mut entries = dropindir
        .list_dir(".")?
        .map(|e| e.map_err(anyhow::Error::msg))
        .collect::<Result<Vec<_>>>()?;
    entries.sort_by(|a, b| a.file_name().cmp(b.file_name()));
    for ent in entries {
        let name = ent.file_name();
        let name = name
            .to_str()
            .ok_or_else(|| anyhow!("Invalid UTF-8: {name:?}"))?;
        if !name.ends_with(".cfg") {
            log::debug!("Ignoring {name}");
            continue;
        }
        writeln!(config, "source $prefix/{name}")?;
        dropindir
            .copy_file_at(name, bootdir, format!("{GRUB2DIR}/{name}"))
            .with_context(|| format!("Copying {name}"))?;
        println!("Installed {name}");
    }

    {
        let post = std::fs::read_to_string(Path::new(CONFIGDIR).join("grub-static-post.cfg"))?;
        config.push_str(post.as_str());
    }

    bootdir
        .write_file_contents(format!("{GRUB2DIR}/grub.cfg"), 0o644, config.as_bytes())
        .context("Copying grub-static.cfg")?;
    println!("Installed: grub.cfg");

    let uuid_path = if write_uuid {
        let target_fs = if boot_is_mount { bootdir } else { target_root };
        let bootfs_meta = crate::filesystem::inspect_filesystem(target_fs, ".")?;
        let bootfs_uuid = bootfs_meta
            .uuid
            .ok_or_else(|| anyhow::anyhow!("Failed to find UUID for boot"))?;
        let grub2_uuid_contents = format!("set BOOT_UUID=\"{bootfs_uuid}\"\n");
        let uuid_path = format!("{GRUB2DIR}/bootuuid.cfg");
        bootdir
            .write_file_contents(&uuid_path, 0o644, grub2_uuid_contents)
            .context("Writing bootuuid.cfg")?;
        Some(uuid_path)
    } else {
        None
    };

    let efidir = efi
        .then(|| {
            target_root
                .sub_dir_optional("boot/efi/EFI")
                .context("Opening /boot/efi/EFI")
        })
        .transpose()?
        .flatten();
    if let Some(efidir) = efidir.as_ref() {
        let vendordir = find_efi_vendordir(efidir, None)?;
        log::debug!("vendordir={:?}", &vendordir);
        let target = &vendordir.join("grub.cfg");
        efidir
            .copy_file(&Path::new(CONFIGDIR).join("grub-static-efi.cfg"), target)
            .context("Copying static EFI")?;
        println!("Installed: {target:?}");
        if let Some(uuid_path) = uuid_path {
            // SAFETY: we always have a filename
            let filename = Path::new(&uuid_path).file_name().unwrap();
            let target = &vendordir.join(filename);
            bootdir
                .copy_file_at(uuid_path, efidir, target)
                .context("Writing bootuuid.cfg to efi dir")?;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore]
    fn test_install() -> Result<()> {
        env_logger::init();
        let td = tempfile::tempdir()?;
        let tdp = td.path();
        let td = openat::Dir::open(tdp)?;
        std::fs::create_dir_all(tdp.join("boot/grub2"))?;
        std::fs::create_dir_all(tdp.join("boot/efi/EFI/BOOT"))?;
        std::fs::create_dir_all(tdp.join("boot/efi/EFI/fedora"))?;
        install(&td, true, false).unwrap();

        assert!(td.exists("boot/grub2/grub.cfg")?);
        assert!(td.exists("boot/efi/EFI/fedora/grub.cfg")?);
        Ok(())
    }

    #[test]
    fn test_find_efi_vendordir() -> Result<()> {
        let td = tempfile::tempdir()?;
        let tdp = td.path();
        let td_updates = tdp.join("usr/lib/bootupd/updates/EFI");
        std::fs::create_dir_all(&td_updates)?;
        std::fs::create_dir_all(td_updates.join("fedora"))?;
        std::fs::write(td_updates.join(format!("fedora/{SHIM}")), "shim data")?;

        assert!(td_updates.join("fedora").join(SHIM).exists());
        std::fs::create_dir_all(td_updates.join("centos"))?;
        std::fs::write(td_updates.join(format!("centos/{SHIM}")), "shim data")?;
        assert!(td_updates.join("centos").join(SHIM).exists());

        std::fs::create_dir_all(tdp.join("EFI/BOOT"))?;
        std::fs::create_dir_all(tdp.join("EFI/dell"))?;
        std::fs::create_dir_all(tdp.join("EFI/fedora"))?;
        let efidir = tdp.join("EFI");
        let td = openat::Dir::open(&efidir)?;

        // error, multiple shim in the image
        let x = find_efi_vendordir(&td, tdp.to_str());
        assert_eq!(x.is_err(), true);

        std::fs::remove_dir_all(td_updates.join("centos"))?;

        std::fs::write(tdp.join("EFI/BOOT").join(SHIM), "boot shim data")?;
        std::fs::write(tdp.join("EFI/dell/foo"), "foo data")?;
        std::fs::write(tdp.join("EFI/fedora/grub.cfg"), "grub config")?;
        std::fs::write(tdp.join("EFI/fedora").join(SHIM), "shim data")?;

        assert!(td.exists(format!("BOOT/{SHIM}"))?);
        assert!(td.exists("dell/foo")?);
        assert!(td.exists("fedora/grub.cfg")?);
        assert!(td.exists(format!("fedora/{SHIM}"))?);
        // successes, match content and {vendor}/shim
        assert_eq!(
            find_efi_vendordir(&td, tdp.to_str())?.to_str(),
            Some("fedora")
        );

        // error, match content and not match path {vendor}/shim
        std::fs::write(tdp.join("EFI/BOOT").join(SHIM), "shim data")?;
        let x = find_efi_vendordir(&td, tdp.to_str());
        assert_eq!(x.is_err(), true);

        // error, not found {vendor}/shim
        std::fs::remove_file(efidir.join("fedora").join(SHIM))?;
        let x = find_efi_vendordir(&td, tdp.to_str());
        assert_eq!(x.is_err(), true);
        std::fs::remove_dir_all(tdp)?;
        Ok(())
    }
}
