use anyhow::{bail, Context, Result};
use fn_error_context::context;
use log::warn;

/// Install the systemd-boot entry files
#[context("Installing systemd-boot entries")]
pub(crate) fn install(
    target_root: &openat::Dir,   // This should be mounted ESP root dir (not /boot inside ESP)
    _write_uuid: bool,
) -> Result<()> {
    let status = std::process::Command::new("bootctl")
        .args([
            "install",
            "--esp-path",
            target_root.recover_path()?.to_str().context("ESP path is not valid UTF-8")?,
        ])
        .status()
        .context("running  install")?;
    warn!("bootctl install status: {}", status);
    if !status.success() {
        bail!("bootctl install failed with status: {}", status);
    }

    Ok(())
}

// use anyhow::{Context, Result};
// use fn_error_context::context;
// use log::warn;
// use std::fs;
// use std::path::Path;

// /// Install the systemd-boot entry files
// #[context("Installing systemd-boot entries")]
// pub(crate) fn install(
//     target_root: &openat::Dir,   // This should be mounted ESP root dir (not /boot inside ESP)
//     _write_uuid: bool,
// ) -> Result<()> {
//     let esp_path = target_root.recover_path()?.to_str().context("ESP path is not valid UTF-8")?.to_string();

//     let dirs = [
//         "EFI/systemd",
//         "EFI/BOOT",
//         "loader",
//         "loader/keys",
//         "loader/entries",
//         "EFI/Linux",
//     ];
//     for dir in dirs.iter() {
//         let full_path = Path::new(&esp_path).join(dir);
//         if !full_path.exists() {
//             fs::create_dir_all(&full_path).context(format!("Creating {}", full_path.display()))?;
//             warn!("Created \"{}\".", full_path.display());
//         }
//     }

//     let src_efi = "/usr/lib/systemd/boot/efi/systemd-bootx64.efi";
//     let dst_systemd = Path::new(&esp_path).join("EFI/systemd/systemd-bootx64.efi");
//     let dst_boot = Path::new(&esp_path).join("EFI/BOOT/BOOTX64.EFI");

//     fs::copy(src_efi, &dst_systemd)
//         .context(format!("Copying {} to {}", src_efi, dst_systemd.display()))?;
//     warn!("Copied \"{}\" to \"{}\".", src_efi, dst_systemd.display());

//     fs::copy(src_efi, &dst_boot)
//         .context(format!("Copying {} to {}", src_efi, dst_boot.display()))?;
//     warn!("Copied \"{}\" to \"{}\".", src_efi, dst_boot.display());

//     Ok(())
// }
