use std::fmt::Write;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use cap_std::fs::{Dir, DirBuilder, DirBuilderExt, MetadataExt};
use cap_std_ext::cap_std;
use cap_std_ext::cap_std::fs::{Permissions, PermissionsExt};
use cap_std_ext::dirext::CapStdExtDirExt;
use fn_error_context::context;

use crate::util;

/// The subdirectory of /boot we use
const GRUB2DIR: &str = "grub2";
const CONFIGDIR: &str = "/usr/lib/bootupd/grub2-static";
const DROPINDIR: &str = "configs.d";

/// Install the static GRUB config files.
#[context("Installing static GRUB configs")]
pub(crate) fn install(
    target_root: &openat::Dir,
    installed_efi_vendor: Option<&str>,
    write_uuid: bool,
) -> Result<()> {
    let target_root = &util::reopen_dir(target_root)?;
    let bootdir = &target_root.open_dir("boot").context("Opening /boot")?;
    let boot_is_mount = {
        let root_dev = target_root.dir_metadata()?.dev();
        let boot_dev = bootdir.dir_metadata()?.dev();
        log::debug!("root_dev={root_dev} boot_dev={boot_dev}");
        root_dev != boot_dev
    };

    if !bootdir.try_exists(GRUB2DIR)? {
        let mut db = DirBuilder::new();
        db.mode(0o700);
        bootdir.create_dir_with(GRUB2DIR, &db)?;
    }

    let mut config = std::fs::read_to_string(Path::new(CONFIGDIR).join("grub-static-pre.cfg"))?;

    let dropindir = Dir::open_ambient_dir(
        &Path::new(CONFIGDIR).join(DROPINDIR),
        cap_std::ambient_authority(),
    )?;
    // Sort the files for reproducibility
    let mut entries = dropindir
        .entries()?
        .map(|e| e.map_err(anyhow::Error::msg))
        .collect::<Result<Vec<_>>>()?;
    // cc https://github.com/rust-lang/rust/issues/85573#issuecomment-2195271304
    entries.sort_by(|a, b| a.file_name().cmp(&b.file_name()));
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
            .copy(name, bootdir, format!("{GRUB2DIR}/{name}"))
            .with_context(|| format!("Copying {name}"))?;
        println!("Installed {name}");
    }

    {
        let post = std::fs::read_to_string(Path::new(CONFIGDIR).join("grub-static-post.cfg"))?;
        config.push_str(post.as_str());
    }

    let rperms = Permissions::from_mode(0o644);
    bootdir
        .atomic_write_with_perms(
            format!("{GRUB2DIR}/grub.cfg"),
            config.as_bytes(),
            rperms.clone(),
        )
        .context("Copying grub-static.cfg")?;
    println!("Installed: grub.cfg");

    let uuid_path = if write_uuid {
        let target_fs = if boot_is_mount { bootdir } else { target_root };
        let target_fs_dir = &util::reopen_legacy_dir(target_fs)?;
        let bootfs_meta = crate::filesystem::inspect_filesystem(target_fs_dir, ".")?;
        let bootfs_uuid = bootfs_meta
            .uuid
            .ok_or_else(|| anyhow::anyhow!("Failed to find UUID for boot"))?;
        let grub2_uuid_contents = format!("set BOOT_UUID=\"{bootfs_uuid}\"\n");
        let uuid_path = format!("{GRUB2DIR}/bootuuid.cfg");
        bootdir
            .atomic_write_with_perms(&uuid_path, grub2_uuid_contents, rperms)
            .context("Writing bootuuid.cfg")?;
        Some(uuid_path)
    } else {
        None
    };

    if let Some(vendordir) = installed_efi_vendor {
        log::debug!("vendordir={:?}", &vendordir);
        let vendor = PathBuf::from(vendordir);
        let target = &vendor.join("grub.cfg");
        let dest_efidir = target_root
            .open_dir_optional("boot/efi/EFI")
            .context("Opening /boot/efi/EFI")?;
        if let Some(efidir) = dest_efidir {
            efidir
                .copy(
                    &Path::new(CONFIGDIR).join("grub-static-efi.cfg"),
                    &efidir,
                    target,
                )
                .context("Copying static EFI")?;
            println!("Installed: {target:?}");
            if let Some(uuid_path) = uuid_path {
                // SAFETY: we always have a filename
                let filename = Path::new(&uuid_path).file_name().unwrap();
                let target = &vendor.join(filename);
                bootdir
                    .copy(uuid_path, &efidir, target)
                    .context("Writing bootuuid.cfg to efi dir")?;
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use openat_ext::OpenatDirExt;

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
        install(&td, Some("fedora"), false).unwrap();

        assert!(td.exists("boot/grub2/grub.cfg")?);
        assert!(td.exists("boot/efi/EFI/fedora/grub.cfg")?);
        Ok(())
    }
}
