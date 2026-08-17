use anyhow::{anyhow, Context};
use cap_std::{ambient_authority, fs::Dir};
use cap_std_ext::dirext::CapStdExtDirExt;
use log::info;
use std::{
    fs::create_dir_all,
    io::{copy, Write},
    path::Path,
};

use super::BootupdVarlinkError;
use crate::{
    bootupd::list_dev_current_root, efi, freezethaw::fsfreeze_thaw_cycle, model::SavedState,
};

/// Find the ESP device matching a given partition UUID
fn find_esp_by_partuuid<'a>(
    devices: &'a [bootc_internal_blockdev::Device],
    partuuid: &str,
) -> Option<&'a bootc_internal_blockdev::Device> {
    devices.iter().find(|d| {
        d.partuuid
            .as_deref()
            .is_some_and(|u| u.eq_ignore_ascii_case(partuuid))
    })
}

/// Validate that a given path is a valid capsule directory
fn validate_capsule_dir(capsule_dir: &Path) -> Result<(), BootupdVarlinkError> {
    // Must be relative
    if capsule_dir.is_absolute() {
        return Err(BootupdVarlinkError::new(
            "capsule_dir must be relative to the ESP".into(),
        ));
    }

    // Must not contain path traversal components
    if capsule_dir
        .components()
        .any(|c| c == std::path::Component::ParentDir)
    {
        return Err(BootupdVarlinkError::new(
            "capsule_dir must not contain '..' components - path traversal not allowed".into(),
        ));
    }

    Ok(())
}

/// See [`super::BootupdVarlinkService::sync_fwupd_updates`]
pub(super) fn _sync_fwupd_updates(
    partuuid: &str,
    capsule_dir: &str,
) -> Result<(), BootupdVarlinkError> {
    if partuuid.is_empty() {
        return Err(BootupdVarlinkError::new(
            "partuuid must not be empty".into(),
        ));
    }
    if capsule_dir.is_empty() {
        return Err(BootupdVarlinkError::new(
            "capsule_dir must not be empty".into(),
        ));
    }

    let capsule_dir = Path::new(capsule_dir);
    validate_capsule_dir(capsule_dir)?;

    let root_device = list_dev_current_root()?;
    let esp_devices = root_device
        .find_colocated_esps()?
        .ok_or_else(|| anyhow!("could not find any co-located ESPs"))?;

    // Find the source ESP (the one fwupd wrote to).
    let primary_device = find_esp_by_partuuid(&esp_devices, partuuid).ok_or_else(|| {
        BootupdVarlinkError::new(format!("No ESP found with partuuid {partuuid}"))
    })?;

    // Avoid running at the same time as bootloader updates
    let sysroot = Dir::open_ambient_dir("/", ambient_authority()).context("opening sysroot")?;
    let _lock = SavedState::acquire_write_lock("/".into(), sysroot)?;

    // Mount primary ESP and find capsule updates dir
    let primary_efi = efi::mount_esp(&primary_device.path()).with_context(|| {
        format!(
            "Creating temp ESP mount for primary ESP (partuuid: {partuuid}) at {:?}",
            primary_device.path(),
        )
    })?;
    let primary_mount = primary_efi.dir.path().to_path_buf();

    let src_capsule_path = primary_mount.join(capsule_dir);
    let src_dir = Dir::open_ambient_dir(&src_capsule_path, ambient_authority())
        .context("opening source capsule dir")?;

    // Sync to every other co-located ESP.
    let mut synced_count = 0;
    for esp in esp_devices.iter() {
        if esp
            .partuuid
            .as_ref()
            .is_some_and(|u| u.eq_ignore_ascii_case(partuuid))
        {
            continue;
        }

        let secondary_efi = efi::mount_esp(&esp.path()).with_context(|| {
            format!(
                "Creating temp ESP mount for secondary ESP (partuuid: {}) at {:?}",
                esp.partuuid.as_deref().unwrap_or("unknown"),
                esp.path(),
            )
        })?;
        let dest_mount = secondary_efi.dir.path().to_path_buf();

        let dest_capsule_path = dest_mount.join(capsule_dir);
        create_dir_all(&dest_capsule_path)
            .with_context(|| format!("creating {dest_capsule_path:?}"))?;

        let dest_dir = Dir::open_ambient_dir(&dest_capsule_path, ambient_authority())
            .context("opening destination capsule dir")?;

        for entry in src_dir.entries().context("reading source capsule dir")? {
            let entry = entry.context("reading dir entry")?;
            let name = entry.file_name();
            dest_dir
                .atomic_replace_with(&name, |dest_file| -> std::io::Result<()> {
                    let mut src_file = src_dir.open(&name)?;
                    copy(&mut src_file, dest_file)?;
                    dest_file.flush()?;
                    dest_file.get_ref().as_file().sync_data()
                })
                .with_context(|| {
                    format!(
                        "syncing {name:?} from ESP {partuuid} to ESP {}",
                        esp.partuuid.as_deref().unwrap_or("unknown")
                    )
                })?;
        }

        fsfreeze_thaw_cycle(
            dest_dir
                .reopen_as_ownedfd()
                .context("reopening dest dir as owned fd")?,
        )?;
        drop(dest_dir);
        drop(secondary_efi);

        synced_count += 1;
    }
    info!("successfully synced {capsule_dir:?} from ESP {partuuid} to {synced_count} colocated ESP(s)");

    drop(src_dir);
    drop(primary_efi);

    Ok(())
}
