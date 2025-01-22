use std::collections::HashMap;
use std::path::Path;
use std::process::Command;
use std::sync::OnceLock;

use crate::util;
use anyhow::{bail, Context, Result};
use bootc_utils::CommandRunExt;
use fn_error_context::context;
use regex::Regex;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug)]
struct BlockDevices {
    blockdevices: Vec<Device>,
}

#[derive(Serialize, Deserialize, Debug)]
struct Device {
    path: String,
    pttype: Option<String>,
    parttype: Option<String>,
    parttypename: Option<String>,
}

impl Device {
    pub(crate) fn is_esp_part(&self) -> bool {
        const ESP_TYPE_GUID: &str = "c12a7328-f81f-11d2-ba4b-00a0c93ec93b";
        if let Some(parttype) = &self.parttype {
            if parttype.to_lowercase() == ESP_TYPE_GUID {
                return true;
            }
        }
        false
    }

    pub(crate) fn is_bios_boot_part(&self) -> bool {
        const BIOS_BOOT_TYPE_GUID: &str = "21686148-6449-6e6f-744e-656564454649";
        if let Some(parttype) = &self.parttype {
            if parttype.to_lowercase() == BIOS_BOOT_TYPE_GUID
                && self.pttype.as_deref() == Some("gpt")
            {
                return true;
            }
        }
        false
    }
}

/// Parse key-value pairs from lsblk --pairs.
/// Newer versions of lsblk support JSON but the one in CentOS 7 doesn't.
fn split_lsblk_line(line: &str) -> HashMap<String, String> {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    let regex = REGEX.get_or_init(|| Regex::new(r#"([A-Z-_]+)="([^"]+)""#).unwrap());
    let mut fields: HashMap<String, String> = HashMap::new();
    for cap in regex.captures_iter(line) {
        fields.insert(cap[1].to_string(), cap[2].to_string());
    }
    fields
}

/// This is a bit fuzzy, but... this function will return every block device in the parent
/// hierarchy of `device` capable of containing other partitions. So e.g. parent devices of type
/// "part" doesn't match, but "disk" and "mpath" does.
pub(crate) fn find_parent_devices(device: &str) -> Result<Vec<String>> {
    let mut cmd = Command::new("lsblk");
    // Older lsblk, e.g. in CentOS 7.6, doesn't support PATH, but --paths option
    cmd.arg("--pairs")
        .arg("--paths")
        .arg("--inverse")
        .arg("--output")
        .arg("NAME,TYPE")
        .arg(device);
    let output = util::cmd_output(&mut cmd)?;
    let mut parents = Vec::new();
    // skip first line, which is the device itself
    for line in output.lines().skip(1) {
        let dev = split_lsblk_line(line);
        let name = dev
            .get("NAME")
            .with_context(|| format!("device in hierarchy of {device} missing NAME"))?;
        let kind = dev
            .get("TYPE")
            .with_context(|| format!("device in hierarchy of {device} missing TYPE"))?;
        if kind == "disk" {
            parents.push(name.clone());
        } else if kind == "mpath" {
            parents.push(name.clone());
            // we don't need to know what disks back the multipath
            break;
        }
    }
    Ok(parents)
}

#[context("get parent devices from mountpoint boot")]
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
    let parent_devices = find_parent_devices(&fsinfo.source)
        .with_context(|| format!("while looking for backing devices of {}", fsinfo.source))?;
    log::debug!("Find parent devices: {parent_devices:?}");
    Ok(parent_devices)
}

// Get single device for the target root
pub fn get_single_device<P: AsRef<Path>>(target_root: P) -> Result<String> {
    let target_root = target_root.as_ref();
    let bootdir = target_root.join("boot");
    if !bootdir.exists() {
        bail!("{} does not exist", bootdir.display());
    }
    let bootdir = openat::Dir::open(&bootdir)?;
    // Run findmnt to get the source path of mount point boot
    let fsinfo = crate::filesystem::inspect_filesystem(&bootdir, ".")?;
    // Find the single parent device of the source path
    let backing_device = {
        let mut dev = fsinfo.source;
        loop {
            log::debug!("Finding parents for {dev}");
            let mut parents = find_parent_devices(&dev)?.into_iter();
            let Some(parent) = parents.next() else {
                break;
            };
            log::debug!("Get {dev} parent: {parent}");
            if let Some(next) = parents.next() {
                anyhow::bail!(
                    "Found multiple parent devices {parent} and {next}; not currently supported"
                );
            }
            dev = parent;
        }
        dev
    };
    Ok(backing_device)
}

#[context("Listing device {device}")]
fn list_dev(device: &str) -> Result<BlockDevices> {
    let devs: BlockDevices = Command::new("lsblk")
        .args([
            "--json",
            "--output",
            "PATH,PTTYPE,PARTTYPE,PARTTYPENAME",
            device,
        ])
        .run_and_parse_json()?;
    Ok(devs)
}

/// Find esp partition on the same device
pub fn get_esp_partition(device: &str) -> Result<Option<String>> {
    let dev = list_dev(&device)?;
    // Find the ESP part on the disk
    for part in dev.blockdevices {
        if part.is_esp_part() {
            return Ok(Some(part.path));
        }
    }
    log::debug!("Not found any esp partition");
    Ok(None)
}

/// Find all ESP partitions on the devices with mountpoint boot
#[allow(dead_code)]
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
    let dev = list_dev(&device)?;
    // Find the BIOS BOOT part on the disk
    for part in dev.blockdevices {
        if part.is_bios_boot_part() {
            return Ok(Some(part.path));
        }
    }
    log::debug!("Not found any bios_boot partition");
    Ok(None)
}

/// Find all bios_boot partitions on the devices with mountpoint boot
#[allow(dead_code)]
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_deserialize_lsblk_output() {
        let data = include_str!("../tests/fixtures/example-lsblk-output.json");
        let devices: BlockDevices =
            serde_json::from_str(&data).expect("JSON was not well-formatted");
        assert_eq!(devices.blockdevices.len(), 7);
        assert_eq!(devices.blockdevices[0].path, "/dev/sr0");
        assert!(devices.blockdevices[0].pttype.is_none());
        assert!(devices.blockdevices[0].parttypename.is_none());
    }
}
