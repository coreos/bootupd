use camino::Utf8Path;
use std::path::Path;

use anyhow::{bail, Context, Result};
use bootc_blockdev::PartitionTable;
use fn_error_context::context;

#[context("get parent devices from mount point boot")]
pub fn get_devices<P: AsRef<Path>>(target_root: P) -> Result<Vec<String>> {
    let target_root = target_root.as_ref();
    let bootdir = target_root.join("boot");
    if !bootdir.exists() {
        bail!("{} does not exist", bootdir.display());
    }
    let bootdir = openat::Dir::open(&bootdir)?;
    // Run findmnt to get the source path of mount point boot
    let fsinfo = crate::filesystem::inspect_filesystem(&bootdir, ".")?;
    // Find the parent devices of the source path
    let parent_devices = bootc_blockdev::find_parent_devices(&fsinfo.source)
        .with_context(|| format!("while looking for backing devices of {}", fsinfo.source))?;
    log::debug!("Find parent devices: {parent_devices:?}");
    Ok(parent_devices)
}

// Get single device for the target root
pub fn get_single_device<P: AsRef<Path>>(target_root: P) -> Result<String> {
    let mut devices = get_devices(&target_root)?.into_iter();
    let Some(parent) = devices.next() else {
        anyhow::bail!("Failed to find parent device");
    };

    if let Some(next) = devices.next() {
        anyhow::bail!("Found multiple parent devices {parent} and {next}; not currently supported");
    }
    Ok(parent)
}

/// Find esp partition on the same device
/// using sfdisk to get partitiontable
pub fn get_esp_partition(device: &str) -> Result<Option<String>> {
    const ESP_TYPE_GUID: &str = "C12A7328-F81F-11D2-BA4B-00A0C93EC93B";
    let device_info: PartitionTable = bootc_blockdev::partitions_of(Utf8Path::new(device))?;
    let esp = device_info
        .partitions
        .into_iter()
        .find(|p| p.parttype.as_str() == ESP_TYPE_GUID);
    if let Some(esp) = esp {
        return Ok(Some(esp.node));
    }
    Ok(None)
}

/// Find all ESP partitions on the devices with mountpoint boot
pub fn find_colocated_esps<P: AsRef<Path>>(target_root: P) -> Result<Vec<String>> {
    // first, get the parent device
    let devices = get_devices(&target_root).with_context(|| "while looking for colocated ESPs")?;

    // now, look for all ESPs on those devices
    let mut esps = Vec::new();
    for device in devices {
        if let Some(esp) = get_esp_partition(&device)? {
            esps.push(esp)
        }
    }
    log::debug!("Find esp partitions: {esps:?}");
    Ok(esps)
}

/// Find bios_boot partition on the same device
pub fn get_bios_boot_partition(device: &str) -> Result<Option<String>> {
    const BIOS_BOOT_TYPE_GUID: &str = "21686148-6449-6E6F-744E-656564454649";
    let device_info = bootc_blockdev::partitions_of(Utf8Path::new(device))?;
    let bios_boot = device_info
        .partitions
        .into_iter()
        .find(|p| p.parttype.as_str() == BIOS_BOOT_TYPE_GUID);
    if let Some(bios_boot) = bios_boot {
        return Ok(Some(bios_boot.node));
    }
    Ok(None)
}

/// Find all bios_boot partitions on the devices with mountpoint boot
pub fn find_colocated_bios_boot<P: AsRef<Path>>(target_root: P) -> Result<Vec<String>> {
    // first, get the parent device
    let devices =
        get_devices(&target_root).with_context(|| "looking for colocated bios_boot parts")?;

    // now, look for all bios_boot parts on those devices
    let mut bios_boots = Vec::new();
    for device in devices {
        if let Some(bios) = get_bios_boot_partition(&device)? {
            bios_boots.push(bios)
        }
    }
    log::debug!("Find bios_boot partitions: {bios_boots:?}");
    Ok(bios_boots)
}
