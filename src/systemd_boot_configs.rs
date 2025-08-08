use anyhow::{Context, Result};
use fn_error_context::context;
use openat_ext::OpenatDirExt;

const SYSTEMD_BOOT_ENTRIES_DIR: &str = "loader/entries";

pub(crate) struct SystemdBootEntry {
    title: String,
    linux: String,
    initrd: Option<String>,
    options: String,
}

/// Install the systemd-boot entry files
#[context("Installing systemd-boot entries")]
pub(crate) fn install(
    target_root: &openat::Dir,   // This should be mounted ESP root dir (not /boot inside ESP)
    write_uuid: bool,
    os_title: &str,
    linux_path: &str,
    initrd_path: Option<&str>,
) -> Result<()> {
    // Ensure /loader/entries exist on ESP root
    if !target_root.exists(SYSTEMD_BOOT_ENTRIES_DIR)? {
        target_root.create_dir(SYSTEMD_BOOT_ENTRIES_DIR, 0o700)?;
    }

    // Inspect root filesystem UUID - for root=UUID=... kernel parameter
    let rootfs_meta = crate::filesystem::inspect_filesystem(target_root, ".")?;
    let root_uuid = rootfs_meta
        .uuid
        .ok_or_else(|| anyhow::anyhow!("Failed to find UUID for root"))?;

    // Compose entry config
    let config = SystemdBootEntry {
        title: os_title.to_string(),
        // For UKI, path is relative to ESP root, e.g. /EFI/ukify.efi
        linux: format!("/EFI/{}", linux_path.trim_start_matches('/')),
        initrd: initrd_path.map(|p| format!("{}/{}", SYSTEMD_BOOT_ENTRIES_DIR, p)),
        options: if write_uuid {
            format!("root=UUID={} quiet", root_uuid)
        } else {
            "quiet".to_string()
        },
    };

    let mut entry_content = format!("title {}\n", config.title);

    if linux_path.ends_with(".efi") {
        // UKI boot entry
        log::warn!("Installing UKI entry: {}", config.linux);
        entry_content.push_str(&format!("efi {}\n", config.linux));
    } else {
        // Kernel/initrd entry
        entry_content.push_str(&format!("linux {}\n", config.linux));
        if let Some(initrd) = &config.initrd {
            entry_content.push_str(&format!("initrd {}\n", initrd));
        }
        entry_content.push_str(&format!("options {}\n", config.options));
    }

    // Write the entry file under /loader/entries/bootupd.conf on ESP root
    let entries_dir = target_root.sub_dir(SYSTEMD_BOOT_ENTRIES_DIR)?;
    entries_dir.write_file_contents(
        "bootupd.conf",
        0o644,
        entry_content.as_bytes(),
    ).context("Writing systemd-boot loader entry")?;
    log::warn!("Installed: {}/bootupd.conf", SYSTEMD_BOOT_ENTRIES_DIR);

    Ok(())
}
