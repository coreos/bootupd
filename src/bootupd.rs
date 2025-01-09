#[cfg(any(target_arch = "x86_64", target_arch = "powerpc64"))]
use crate::bios;
use crate::component;
use crate::component::{Component, ValidationResult};
use crate::coreos;
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
use crate::efi;
use crate::model::{ComponentStatus, ComponentUpdatable, ContentMetadata, SavedState, Status};
use crate::util;
use anyhow::{anyhow, Context, Result};
use clap::crate_version;
use fn_error_context::context;
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::collections::BTreeMap;
use std::fs::{self, File};
use std::path::{Path, PathBuf};

pub(crate) enum ConfigMode {
    None,
    Static,
    WithUUID,
}

impl ConfigMode {
    pub(crate) fn enabled_with_uuid(&self) -> Option<bool> {
        match self {
            ConfigMode::None => None,
            ConfigMode::Static => Some(false),
            ConfigMode::WithUUID => Some(true),
        }
    }
}

pub(crate) fn install(
    source_root: &str,
    dest_root: &str,
    device: Option<&str>,
    configs: ConfigMode,
    update_firmware: bool,
    target_components: Option<&[String]>,
    auto_components: bool,
) -> Result<()> {
    // TODO: Change this to an Option<&str>; though this probably balloons into having
    // DeviceComponent and FileBasedComponent
    let device = device.unwrap_or("");
    let source_root = openat::Dir::open(source_root).context("Opening source root")?;
    SavedState::ensure_not_present(dest_root)
        .context("failed to install, invalid re-install attempted")?;

    let all_components = get_components_impl(auto_components);
    if all_components.is_empty() {
        println!("No components available for this platform.");
        return Ok(());
    }
    let target_components = if let Some(target_components) = target_components {
        // Checked by CLI parser
        assert!(!auto_components);
        target_components
            .iter()
            .map(|name| {
                all_components
                    .get(name.as_str())
                    .ok_or_else(|| anyhow!("Unknown component: {name}"))
            })
            .collect::<Result<Vec<_>>>()?
    } else {
        all_components.values().collect()
    };

    if target_components.is_empty() && !auto_components {
        anyhow::bail!("No components specified");
    }

    let mut state = SavedState::default();
    let mut installed_efi_vendor = None;
    for &component in target_components.iter() {
        // skip for BIOS if device is empty
        if component.name() == "BIOS" && device.is_empty() {
            println!(
                "Skip installing component {} without target device",
                component.name()
            );
            continue;
        }

        let meta = component
            .install(&source_root, dest_root, device, update_firmware)
            .with_context(|| format!("installing component {}", component.name()))?;
        log::info!("Installed {} {}", component.name(), meta.meta.version);
        state.installed.insert(component.name().into(), meta);
        // Yes this is a hack...the Component thing just turns out to be too generic.
        if let Some(vendor) = component.get_efi_vendor(&source_root)? {
            assert!(installed_efi_vendor.is_none());
            installed_efi_vendor = Some(vendor);
        }
    }
    let sysroot = &openat::Dir::open(dest_root)?;

    match configs.enabled_with_uuid() {
        Some(uuid) => {
            let self_bin_meta =
                std::fs::metadata("/proc/self/exe").context("Querying self meta")?;
            let self_meta = ContentMetadata {
                timestamp: self_bin_meta.modified()?.into(),
                version: crate_version!().into(),
            };
            state.static_configs = Some(self_meta);
            #[cfg(any(
                target_arch = "x86_64",
                target_arch = "aarch64",
                target_arch = "powerpc64"
            ))]
            crate::grubconfigs::install(sysroot, installed_efi_vendor.as_deref(), uuid)?;
            // On other architectures, assume that there's nothing to do.
        }
        None => {}
    }

    // Unmount the ESP, etc.
    drop(target_components);

    let mut state_guard =
        SavedState::unlocked(sysroot.try_clone()?).context("failed to acquire write lock")?;
    state_guard
        .update_state(&state)
        .context("failed to update state")?;

    Ok(())
}

type Components = BTreeMap<&'static str, Box<dyn Component>>;

#[allow(clippy::box_default)]
/// Return the set of known components; if `auto` is specified then the system
/// filters to the target booted state.
pub(crate) fn get_components_impl(auto: bool) -> Components {
    let mut components = BTreeMap::new();

    fn insert_component(components: &mut Components, component: Box<dyn Component>) {
        components.insert(component.name(), component);
    }

    #[cfg(target_arch = "x86_64")]
    {
        if auto {
            let is_efi_booted = crate::efi::is_efi_booted().unwrap();
            log::info!(
                "System boot method: {}",
                if is_efi_booted { "EFI" } else { "BIOS" }
            );
            if is_efi_booted {
                insert_component(&mut components, Box::new(efi::Efi::default()));
            } else {
                insert_component(&mut components, Box::new(bios::Bios::default()));
            }
        } else {
            insert_component(&mut components, Box::new(bios::Bios::default()));
            insert_component(&mut components, Box::new(efi::Efi::default()));
        }
    }
    #[cfg(target_arch = "aarch64")]
    insert_component(&mut components, Box::new(efi::Efi::default()));

    #[cfg(target_arch = "powerpc64")]
    insert_component(&mut components, Box::new(bios::Bios::default()));

    components
}

pub(crate) fn get_components() -> Components {
    get_components_impl(false)
}

pub(crate) fn generate_update_metadata(sysroot_path: &str) -> Result<()> {
    // create bootupd update dir which will save component metadata files for both components
    let updates_dir = Path::new(sysroot_path).join(crate::model::BOOTUPD_UPDATES_DIR);
    std::fs::create_dir_all(&updates_dir)
        .with_context(|| format!("Failed to create updates dir {:?}", &updates_dir))?;
    for component in get_components().values() {
        let v = component.generate_update_metadata(sysroot_path)?;
        println!(
            "Generated update layout for {}: {}",
            component.name(),
            v.version,
        );
    }

    Ok(())
}

/// Return value from daemon → client for component update
#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum ComponentUpdateResult {
    AtLatestVersion,
    Updated {
        previous: ContentMetadata,
        interrupted: Option<ContentMetadata>,
        new: ContentMetadata,
    },
}

fn ensure_writable_boot() -> Result<()> {
    util::ensure_writable_mount("/boot")
}

/// daemon implementation of component update
pub(crate) fn update(name: &str) -> Result<ComponentUpdateResult> {
    let mut state = SavedState::load_from_disk("/")?.unwrap_or_default();
    let component = component::new_from_name(name)?;
    let inst = if let Some(inst) = state.installed.get(name) {
        inst.clone()
    } else {
        anyhow::bail!("Component {} is not installed", name);
    };
    let sysroot = openat::Dir::open("/")?;
    let update = component.query_update(&sysroot)?;
    let update = match update.as_ref() {
        Some(p) if inst.meta.can_upgrade_to(p) => p,
        _ => return Ok(ComponentUpdateResult::AtLatestVersion),
    };

    ensure_writable_boot()?;

    let mut pending_container = state.pending.take().unwrap_or_default();
    let interrupted = pending_container.get(component.name()).cloned();
    pending_container.insert(component.name().into(), update.clone());
    let mut state_guard =
        SavedState::acquire_write_lock(sysroot).context("Failed to acquire write lock")?;
    state_guard
        .update_state(&state)
        .context("Failed to update state")?;

    let newinst = component
        .run_update(&state_guard.sysroot, &inst)
        .with_context(|| format!("Failed to update {}", component.name()))?;
    state.installed.insert(component.name().into(), newinst);
    pending_container.remove(component.name());
    state_guard.update_state(&state)?;

    Ok(ComponentUpdateResult::Updated {
        previous: inst.meta,
        interrupted,
        new: update.clone(),
    })
}

/// daemon implementation of component adoption
pub(crate) fn adopt_and_update(name: &str) -> Result<ContentMetadata> {
    let sysroot = openat::Dir::open("/")?;
    let mut state = SavedState::load_from_disk("/")?.unwrap_or_default();
    let component = component::new_from_name(name)?;
    if state.installed.contains_key(name) {
        anyhow::bail!("Component {} is already installed", name);
    };

    ensure_writable_boot()?;

    let Some(update) = component.query_update(&sysroot)? else {
        anyhow::bail!("Component {} has no available update", name);
    };
    let mut state_guard =
        SavedState::acquire_write_lock(sysroot).context("Failed to acquire write lock")?;

    let inst = component
        .adopt_update(&state_guard.sysroot, &update)
        .context("Failed adopt and update")?;
    state.installed.insert(component.name().into(), inst);

    state_guard.update_state(&state)?;
    Ok(update)
}

/// daemon implementation of component validate
pub(crate) fn validate(name: &str) -> Result<ValidationResult> {
    let state = SavedState::load_from_disk("/")?.unwrap_or_default();
    let component = component::new_from_name(name)?;
    let Some(inst) = state.installed.get(name) else {
        anyhow::bail!("Component {} is not installed", name);
    };
    component.validate(inst)
}

pub(crate) fn status() -> Result<Status> {
    let mut ret: Status = Default::default();
    let mut known_components = get_components();
    let sysroot = openat::Dir::open("/")?;
    let state = SavedState::load_from_disk("/")?;
    if let Some(state) = state {
        for (name, ic) in state.installed.iter() {
            log::trace!("Gathering status for installed component: {}", name);
            let component = known_components
                .remove(name.as_str())
                .ok_or_else(|| anyhow!("Unknown component installed: {}", name))?;
            let component = component.as_ref();
            let interrupted = state.pending.as_ref().and_then(|p| p.get(name.as_str()));
            let update = component.query_update(&sysroot)?;
            let updatable = ComponentUpdatable::from_metadata(&ic.meta, update.as_ref());
            let adopted_from = ic.adopted_from.clone();
            ret.components.insert(
                name.to_string(),
                ComponentStatus {
                    installed: ic.meta.clone(),
                    interrupted: interrupted.cloned(),
                    update,
                    updatable,
                    adopted_from,
                },
            );
        }
    } else {
        log::trace!("No saved state");
    }

    // Process the remaining components not installed
    log::trace!("Remaining known components: {}", known_components.len());
    for (name, component) in known_components {
        if let Some(adopt_ver) = component.query_adopt()? {
            ret.adoptable.insert(name.to_string(), adopt_ver);
        } else {
            log::trace!("Not adoptable: {}", name);
        }
    }

    Ok(ret)
}

pub(crate) fn print_status_avail(status: &Status) -> Result<()> {
    let mut avail = Vec::new();
    for (name, component) in status.components.iter() {
        if let ComponentUpdatable::Upgradable = component.updatable {
            avail.push(name.as_str());
        }
    }
    for (name, adoptable) in status.adoptable.iter() {
        if adoptable.confident {
            avail.push(name.as_str());
        }
    }
    if !avail.is_empty() {
        println!("Updates available: {}", avail.join(" "));
    }
    Ok(())
}

pub(crate) fn print_status(status: &Status) -> Result<()> {
    if status.components.is_empty() {
        println!("No components installed.");
    }
    for (name, component) in status.components.iter() {
        println!("Component {}", name);
        println!("  Installed: {}", component.installed.version);

        if let Some(i) = component.interrupted.as_ref() {
            println!(
                "  WARNING: Previous update to {} was interrupted",
                i.version
            );
        }
        let msg = match component.updatable {
            ComponentUpdatable::NoUpdateAvailable => Cow::Borrowed("No update found"),
            ComponentUpdatable::AtLatestVersion => Cow::Borrowed("At latest version"),
            ComponentUpdatable::WouldDowngrade => Cow::Borrowed("Ignoring downgrade"),
            ComponentUpdatable::Upgradable => Cow::Owned(format!(
                "Available: {}",
                component.update.as_ref().expect("update").version
            )),
        };
        println!("  Update: {}", msg);
    }

    if status.adoptable.is_empty() {
        println!("No components are adoptable.");
    }
    for (name, adopt) in status.adoptable.iter() {
        let ver = &adopt.version.version;
        if adopt.confident {
            println!("Detected: {}: {}", name, ver);
        } else {
            println!("Adoptable: {}: {}", name, ver);
        }
    }

    if let Some(coreos_aleph) = coreos::get_aleph_version(Path::new("/"))? {
        println!("CoreOS aleph version: {}", coreos_aleph.aleph.version);
    }

    #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
    {
        let boot_method = if efi::is_efi_booted()? { "EFI" } else { "BIOS" };
        println!("Boot method: {}", boot_method);
    }

    Ok(())
}

pub(crate) fn client_run_update() -> Result<()> {
    crate::try_fail_point!("update");
    let status: Status = status()?;
    if status.components.is_empty() && status.adoptable.is_empty() {
        println!("No components installed.");
        return Ok(());
    }
    let mut updated = false;
    for (name, cstatus) in status.components.iter() {
        match cstatus.updatable {
            ComponentUpdatable::Upgradable => {}
            _ => continue,
        };
        match update(name)? {
            ComponentUpdateResult::AtLatestVersion => {
                // Shouldn't happen unless we raced with another client
                eprintln!(
                    "warning: Expected update for {}, raced with a different client?",
                    name
                );
                continue;
            }
            ComponentUpdateResult::Updated {
                previous,
                interrupted,
                new,
            } => {
                if let Some(i) = interrupted {
                    eprintln!(
                        "warning: Continued from previous interrupted update: {}",
                        i.version,
                    );
                }
                println!("Previous {}: {}", name, previous.version);
                println!("Updated {}: {}", name, new.version);
            }
        }
        updated = true;
    }
    for (name, adoptable) in status.adoptable.iter() {
        if adoptable.confident {
            let r: ContentMetadata = adopt_and_update(name)?;
            println!("Adopted and updated: {}: {}", name, r.version);
            updated = true;
        } else {
            println!("Component {} requires explicit adopt-and-update", name);
        }
    }
    if !updated {
        println!("No update available for any component.");
    }
    Ok(())
}

pub(crate) fn client_run_adopt_and_update() -> Result<()> {
    let status: Status = status()?;
    if status.adoptable.is_empty() {
        println!("No components are adoptable.");
    } else {
        for (name, _) in status.adoptable.iter() {
            let r: ContentMetadata = adopt_and_update(name)?;
            println!("Adopted and updated: {}: {}", name, r.version);
        }
    }
    Ok(())
}

pub(crate) fn client_run_validate() -> Result<()> {
    let status: Status = status()?;
    if status.components.is_empty() {
        println!("No components installed.");
        return Ok(());
    }
    let mut caught_validation_error = false;
    for (name, _) in status.components.iter() {
        match validate(name)? {
            ValidationResult::Valid => {
                println!("Validated: {}", name);
            }
            ValidationResult::Skip => {
                println!("Skipped: {}", name);
            }
            ValidationResult::Errors(errs) => {
                for err in errs {
                    eprintln!("{}", err);
                }
                caught_validation_error = true;
            }
        }
    }
    if caught_validation_error {
        anyhow::bail!("Caught validation errors");
    }
    Ok(())
}

#[context("Migrating to static grub config")]
pub(crate) fn client_run_migrate() -> Result<()> {
    // Used to condition execution of this unit at the systemd level
    let stamp_file = "/boot/.bootupd-static-migration-complete";

    // Did we already complete the migration?
    let mut ostree_cmd = std::process::Command::new("ostree");
    let result = ostree_cmd
        .args([
            "config",
            "--repo=/sysroot/ostree/repo",
            "get",
            "sysroot.bootloader",
        ])
        .output()
        .context("Querying ostree sysroot.bootloader")?;
    if !result.status.success() {
        // ostree will exit with a non zero return code if the key does not exists
        println!("ostree repo 'sysroot.bootloader' config option not set yet.");
    } else {
        let bootloader = String::from_utf8(result.stdout)
            .with_context(|| "decoding as UTF-8 output of ostree command")?;
        if bootloader.trim_end() == "none" {
            println!("ostree repo 'sysroot.bootloader' config option already set to 'none'.");
            println!("Assuming that the migration is already complete.");
            File::create(stamp_file)?;
            return Ok(());
        }
        println!(
            "ostree repo 'sysroot.bootloader' config currently set to: {}",
            bootloader.trim_end()
        );
    }

    // Remount /boot read write just for this unit (we are called in a slave mount namespace by systemd)
    ensure_writable_boot()?;

    let grub_config_dir = PathBuf::from("/boot/grub2");
    let dirfd = openat::Dir::open(&grub_config_dir).context("Opening /boot/grub2")?;

    // Migrate /boot/grub2/grub.cfg to a static GRUB config if it is a symlink
    let grub_config_filename = PathBuf::from("/boot/grub2/grub.cfg");
    match dirfd.read_link("grub.cfg") {
        Err(_) => {
            println!(
                "'{}' is not a symlink. Nothing to migrate.",
                grub_config_filename.display()
            );
        }
        Ok(path) => {
            println!("Migrating to a static GRUB config...");

            // Resolve symlink location
            let mut current_config = grub_config_dir.clone();
            current_config.push(path);

            // Backup the current GRUB config which is hopefully working right now
            let backup_config = PathBuf::from("/boot/grub2/grub.cfg.backup");
            println!(
                "Creating a backup of the current GRUB config '{}' in '{}'...",
                current_config.display(),
                backup_config.display()
            );
            fs::copy(&current_config, &backup_config).context("Failed to backup GRUB config")?;

            // Copy it again alongside the current symlink
            let current_config_copy = PathBuf::from("/boot/grub2/grub.cfg.current");
            fs::copy(&current_config, &current_config_copy)
                .context("Failed to copy the current GRUB config")?;

            // Atomically exchange the configs
            dirfd
                .local_exchange("grub.cfg.current", "grub.cfg")
                .context("Failed to exchange symlink with current GRUB config")?;

            // Remove the now unused symlink (optional cleanup, ignore any failures)
            _ = dirfd.remove_file("grub.cfg.current");

            println!("GRUB config symlink successfully replaced with the current config.");
        }
    };

    // If /etc/default/grub exists then we have to force the regeneration of the
    // GRUB config to remove the ostree entries that duplicates the BLS ones
    let grub_default = PathBuf::from("/etc/default/grub");
    if grub_default.exists() {
        println!("Marking bootloader as BLS capable...");
        File::create("/boot/grub2/.grub2-blscfg-supported")
            .context("Failed to mark bootloader as BLS capable")?;

        println!("Regenerating GRUB config with only BLS configs...");
        let status = std::process::Command::new("grub2-mkconfig")
            .arg("-o")
            .arg(grub_config_filename)
            .status()?;
        if !status.success() {
            anyhow::bail!("Failed to regenerate GRUB config");
        }
    }

    println!("Setting up 'sysroot.bootloader' to 'none' in ostree repo config...");
    let status = std::process::Command::new("ostree")
        .args([
            "config",
            "--repo=/sysroot/ostree/repo",
            "set",
            "sysroot.bootloader",
            "none",
        ])
        .status()?;
    if !status.success() {
        anyhow::bail!("Failed to set 'sysroot.bootloader' to 'none' in ostree repo config");
    }

    // Migration complete, let's write the stamp file
    File::create(stamp_file)?;

    println!("Static GRUB config migration completed successfully!");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_failpoint_update() {
        let guard = fail::FailScenario::setup();
        fail::cfg("update", "return").unwrap();
        let r = client_run_update();
        assert_eq!(r.is_err(), true);
        guard.teardown();
    }
}
