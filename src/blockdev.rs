use camino::Utf8Path;
use std::path::Path;

use anyhow::{Context, Result};
use fn_error_context::context;

use crate::bootc_blockdev;

#[context("get parent devices from mount point boot or sysroot")]
pub fn get_devices<P: AsRef<Path>>(target_root: P) -> Result<Vec<String>> {
    let target_root = target_root.as_ref();
    let mut source = None;

    for path in ["boot", "sysroot"] {
        let target_path = target_root.join(path);
        if !target_path.exists() {
            continue;
        }

        let target_dir = openat::Dir::open(&target_path)
            .with_context(|| format!("Opening {}", target_path.display()))?;
        if let Ok(fsinfo) = crate::filesystem::inspect_filesystem(&target_dir, ".") {
            source = Some(fsinfo.source);
            break;
        }
    }

    let source = match source {
        Some(s) => s,
        None => anyhow::bail!("Failed to inspect filesystem from boot or sysroot"),
    };

    // Find the parent devices of the source path
    let parent_devices = bootc_blockdev::find_parent_devices(&source)
        .with_context(|| format!("While looking for backing devices of {}", source))?;
    log::debug!("Found parent devices: {parent_devices:?}");
    Ok(parent_devices)
}

/// Find esp partition on the same device
/// using sfdisk to get partitiontable
pub fn get_esp_partition(device: &str) -> Result<Option<String>> {
    const ESP_TYPE_GUID: &str = "C12A7328-F81F-11D2-BA4B-00A0C93EC93B";
    let device_info: bootc_blockdev::PartitionTable =
        bootc_blockdev::partitions_of(Utf8Path::new(device))?;
    let esp = device_info
        .partitions
        .into_iter()
        .find(|p| p.parttype.as_str() == ESP_TYPE_GUID);
    if let Some(esp) = esp {
        return Ok(Some(esp.node));
    }
    Ok(None)
}

/// Find all ESP partitions on the devices
pub fn find_colocated_esps(devices: &Vec<String>) -> Result<Option<Vec<String>>> {
    // look for all ESPs on those devices
    let mut esps = Vec::new();
    for device in devices {
        if let Some(esp) = get_esp_partition(&device)? {
            esps.push(esp)
        }
    }
    if esps.is_empty() {
        return Ok(None);
    }
    log::debug!("Found esp partitions: {esps:?}");
    Ok(Some(esps))
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

/// Find all bios_boot partitions on the devices
pub fn find_colocated_bios_boot(devices: &Vec<String>) -> Result<Option<Vec<String>>> {
    // look for all bios_boot parts on those devices
    let mut bios_boots = Vec::new();
    for device in devices {
        if let Some(bios) = get_bios_boot_partition(&device)? {
            bios_boots.push(bios)
        }
    }
    if bios_boots.is_empty() {
        return Ok(None);
    }
    log::debug!("Found bios_boot partitions: {bios_boots:?}");
    Ok(Some(bios_boots))
}
