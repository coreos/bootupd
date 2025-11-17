#[cfg(any(target_arch = "x86_64", target_arch = "powerpc64"))]
use crate::bios;
use crate::component;
use crate::component::{Component, ValidationResult};
use crate::coreos;
#[cfg(any(
    target_arch = "x86_64",
    target_arch = "aarch64",
    target_arch = "riscv64"
))]
use crate::efi;
use crate::freezethaw::fsfreeze_thaw_cycle;
use crate::model::{ComponentStatus, ComponentUpdatable, ContentMetadata, SavedState, Status};
use crate::{ostreeutil, util};
use anyhow::{anyhow, Context, Result};
use camino::{Utf8Path, Utf8PathBuf};
use clap::crate_version;
use fn_error_context::context;
use libc::mode_t;
use libc::{S_IRGRP, S_IROTH, S_IRUSR, S_IWUSR};
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, BufWriter, Write};
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
    let source_root_dir = openat::Dir::open(source_root).context("Opening source root")?;
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

        // skip components that don't have an update metadata
        if component.query_update(&source_root_dir)?.is_none() {
            println!(
                "Skip installing component {} without update metadata",
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
        if let Some(vendor) = component.get_efi_vendor(&Path::new(source_root))? {
            assert!(installed_efi_vendor.is_none());
            installed_efi_vendor = Some(vendor);
        }
    }
    let sysroot = &openat::Dir::open(dest_root)?;

    match configs.enabled_with_uuid() {
        Some(uuid) => {
            let meta = get_static_config_meta()?;
            state.static_configs = Some(meta);
            #[cfg(any(
                target_arch = "x86_64",
                target_arch = "aarch64",
                target_arch = "powerpc64",
                target_arch = "riscv64"
            ))]
            crate::grubconfigs::install(
                sysroot,
                Some(&source_root_dir),
                installed_efi_vendor.as_deref(),
                uuid,
            )?;
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

#[context("Get static config metadata")]
fn get_static_config_meta() -> Result<ContentMetadata> {
    let self_bin_meta = std::fs::metadata("/proc/self/exe").context("Querying self meta")?;
    let self_meta = ContentMetadata {
        timestamp: self_bin_meta.modified()?.into(),
        version: crate_version!().into(),
        versions: None,
    };
    Ok(self_meta)
}

type Components = BTreeMap<&'static str, Box<dyn Component>>;

#[allow(clippy::box_default)]
/// Return the set of known components; if `auto` is specified then the system
/// filters to the target booted state.
pub(crate) fn get_components_impl(_auto: bool) -> Components {
    let mut components = BTreeMap::new();

    fn insert_component(components: &mut Components, component: Box<dyn Component>) {
        components.insert(component.name(), component);
    }

    #[cfg(target_arch = "x86_64")]
    {
        if _auto {
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
    #[cfg(any(target_arch = "aarch64", target_arch = "riscv64"))]
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
        if let Some(v) = component.generate_update_metadata(sysroot_path)? {
            println!(
                "Generated update layout for {}: {}",
                component.name(),
                v.version,
            );
        } else {
            println!(
                "Generating update layout for {} was not possible, skipping.",
                component.name(),
            );
        }
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
pub(crate) fn update(name: &str, rootcxt: &RootContext) -> Result<ComponentUpdateResult> {
    let mut state = SavedState::load_from_disk("/")?.unwrap_or_default();
    let component = component::new_from_name(name)?;
    let inst = if let Some(inst) = state.installed.get(name) {
        inst.clone()
    } else {
        anyhow::bail!("Component {} is not installed", name);
    };
    let sysroot = &rootcxt.sysroot;
    let update = component.query_update(sysroot)?;
    let update = match update.as_ref() {
        Some(p) => match inst.meta.can_upgrade_to(p) {
            std::cmp::Ordering::Less => p, // current < available -> upgrade
            _ => return Ok(ComponentUpdateResult::AtLatestVersion),
        },
        None => return Ok(ComponentUpdateResult::AtLatestVersion),
    };

    ensure_writable_boot()?;

    let mut pending_container = state.pending.take().unwrap_or_default();
    let interrupted = pending_container.get(component.name()).cloned();
    pending_container.insert(component.name().into(), update.clone());
    let sysroot = sysroot.try_clone()?;
    let mut state_guard =
        SavedState::acquire_write_lock(sysroot).context("Failed to acquire write lock")?;
    state_guard
        .update_state(&state)
        .context("Failed to update state")?;

    let newinst = component
        .run_update(rootcxt, &inst)
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
pub(crate) fn adopt_and_update(
    name: &str,
    rootcxt: &RootContext,
    with_static_config: bool,
) -> Result<Option<ContentMetadata>> {
    let sysroot = &rootcxt.sysroot;
    let mut state = SavedState::load_from_disk("/")?.unwrap_or_default();
    let component = component::new_from_name(name)?;
    if state.installed.contains_key(name) {
        anyhow::bail!("Component {} is already installed", name);
    };

    ensure_writable_boot()?;

    let Some(update) = component.query_update(sysroot)? else {
        anyhow::bail!("Component {} has no available update", name);
    };

    let sysroot = sysroot.try_clone()?;
    let mut state_guard =
        SavedState::acquire_write_lock(sysroot).context("Failed to acquire write lock")?;

    let inst = component
        .adopt_update(&rootcxt, &update, with_static_config)
        .context("Failed adopt and update")?;
    if let Some(inst) = inst {
        state.installed.insert(component.name().into(), inst);
        // Set static_configs metadata and save
        if with_static_config && state.static_configs.is_none() {
            let meta = get_static_config_meta()?;
            state.static_configs = Some(meta);
            // Set bootloader to none
            ostreeutil::set_ostree_bootloader("none")?;

            println!("Static GRUB configuration has been adopted successfully.");
        }
        state_guard.update_state(&state)?;
        return Ok(Some(update));
    } else {
        // Nothing adopted, skip
        log::info!("Component '{}' skipped adoption", component.name());
        return Ok(None);
    }
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
    for (name, _) in known_components {
        // To determine if not-installed components can be adopted:
        //
        // `query_adopt_state()` checks for existing installation state,
        // such as a `version` in `/sysroot/.coreos-aleph-version.json`,
        // or the presence of `/ostree/deploy`.
        //
        // `component.query_adopt()` performs additional checks,
        // including hardware/device requirements.
        // For example, it will skip BIOS adoption if the system is booted via EFI
        // and lacks a BIOS_BOOT partition.
        //
        // Once a component is determined to be adoptable, it is added to the
        // adoptable list, and adoption proceeds automatically.
        //
        // Therefore, calling `query_adopt_state()` alone is sufficient.
        if let Some(adopt_ver) = component::query_adopt_state()? {
            let component = component::new_from_name(&name)?;
            // Skip if the update metadata could not be found
            if component.query_update(&sysroot)?.is_none() {
                continue;
            };
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

    #[cfg(any(
        target_arch = "x86_64",
        target_arch = "aarch64",
        target_arch = "riscv64"
    ))]
    {
        let boot_method = if efi::is_efi_booted()? { "EFI" } else { "BIOS" };
        println!("Boot method: {}", boot_method);
    }

    Ok(())
}

pub struct RootContext {
    pub sysroot: openat::Dir,
    pub path: Utf8PathBuf,
    pub devices: Vec<String>,
}

impl RootContext {
    fn new(sysroot: openat::Dir, path: &str, devices: Vec<String>) -> Self {
        Self {
            sysroot,
            path: Utf8Path::new(path).into(),
            devices,
        }
    }
}

/// Initialize parent devices to prepare the update
fn prep_before_update() -> Result<RootContext> {
    let path = "/";
    let sysroot = openat::Dir::open(path).context("Opening root dir")?;
    let devices = crate::blockdev::get_devices(path).context("get parent devices")?;
    Ok(RootContext::new(sysroot, path, devices))
}

pub(crate) fn client_run_update() -> Result<()> {
    crate::try_fail_point!("update");
    let rootcxt = prep_before_update()?;
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
        match update(name, &rootcxt)? {
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
            if let Some(r) = adopt_and_update(name, &rootcxt, false)? {
                println!("Adopted and updated: {}: {}", name, r.version);
                updated = true;
            }
        } else {
            println!("Component {} requires explicit adopt-and-update", name);
        }
    }
    if !updated {
        println!("No update available for any component.");
    }
    Ok(())
}

pub(crate) fn client_run_adopt_and_update(with_static_config: bool) -> Result<()> {
    let rootcxt = prep_before_update()?;
    let status: Status = status()?;
    if status.adoptable.is_empty() {
        println!("No components are adoptable.");
    } else {
        for (name, _) in status.adoptable.iter() {
            if let Some(r) = adopt_and_update(name, &rootcxt, with_static_config)? {
                println!("Adopted and updated: {}: {}", name, r.version);
            }
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

#[context("Migrating to a static GRUB config")]
pub(crate) fn client_run_migrate_static_grub_config() -> Result<()> {
    // Did we already complete the migration?
    // We need to migrate if bootloader is not none (or not set)
    if let Some(bootloader) = ostreeutil::get_ostree_bootloader()? {
        if bootloader == "none" {
            println!("Already using a static GRUB config");
            return Ok(());
        }
        println!(
            "ostree repo 'sysroot.bootloader' config option is currently set to: '{}'",
            bootloader
        );
    } else {
        println!("ostree repo 'sysroot.bootloader' config option is not set yet");
    }

    // Remount /boot read write just for this unit (we are called in a slave mount namespace by systemd)
    ensure_writable_boot()?;

    let grub_config_dir = PathBuf::from("/boot/grub2");
    let dirfd = openat::Dir::open(&grub_config_dir).context("Opening /boot/grub2")?;

    // We mark the bootloader as BLS capable to disable the ostree-grub2 logic.
    // We can do that as we know that we are run after the bootloader has been
    // updated and all recent GRUB2 versions support reading BLS configs.
    // Ignore errors as this is not critical. This is a safety net if a user
    // manually overwrites the (soon) static GRUB config by calling `grub2-mkconfig`.
    // We need this until we can rely on ostree-grub2 being removed from the image.
    println!("Marking bootloader as BLS capable...");
    _ = File::create("/boot/grub2/.grub2-blscfg-supported");

    // Migrate /boot/grub2/grub.cfg to a static GRUB config if it is a symlink
    let grub_config_filename = PathBuf::from("/boot/grub2/grub.cfg");
    match dirfd.read_link("grub.cfg") {
        Err(_) => {
            println!(
                "'{}' is not a symlink, nothing to migrate",
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

            // Read the current config, strip the ostree generated GRUB entries and
            // write the result to a temporary file
            println!("Stripping ostree generated entries from GRUB config...");
            let stripped_config = "grub.cfg.stripped";
            let current_config_file =
                File::open(current_config).context("Could not open current GRUB config")?;
            let content = BufReader::new(current_config_file);

            strip_grub_config_file(content, &dirfd, stripped_config)?;

            // Atomically replace the symlink
            dirfd
                .local_rename(stripped_config, "grub.cfg")
                .context("Failed to replace symlink with current GRUB config")?;

            fsfreeze_thaw_cycle(dirfd.open_file(".")?)?;

            println!("GRUB config symlink successfully replaced with the current config");
        }
    };

    println!("Setting 'sysroot.bootloader' to 'none' in ostree repo config...");
    ostreeutil::set_ostree_bootloader("none")?;

    println!("Static GRUB config migration completed successfully");
    Ok(())
}

/// Writes a stripped GRUB config to `stripped_config_name`, removing lines between
/// `### BEGIN /etc/grub.d/15_ostree ###` and `### END /etc/grub.d/15_ostree ###`.
fn strip_grub_config_file(
    current_config_content: impl BufRead,
    dirfd: &openat::Dir,
    stripped_config_name: &str,
) -> Result<()> {
    // mode = -rw-r--r-- (644)
    let mut writer = BufWriter::new(
        dirfd
            .write_file(
                stripped_config_name,
                (S_IRUSR | S_IWUSR | S_IRGRP | S_IROTH) as mode_t,
            )
            .context("Failed to open temporary GRUB config")?,
    );

    let mut skip = false;
    for line in current_config_content.lines() {
        let line = line.context("Failed to read line from GRUB config")?;
        if line == "### END /etc/grub.d/15_ostree ###" {
            skip = false;
            continue;
        }
        if skip {
            continue;
        }
        if line == "### BEGIN /etc/grub.d/15_ostree ###" {
            skip = true;
            continue;
        }
        writer
            .write_all(line.as_bytes())
            .context("Failed to write stripped GRUB config")?;
        writer
            .write_all(b"\n")
            .context("Failed to write stripped GRUB config")?;
    }

    writer
        .into_inner()
        .context("Failed to flush stripped GRUB config")?
        .sync_data()
        .context("Failed to sync stripped GRUB config")?;

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

    #[test]
    fn test_strip_grub_config_file() -> Result<()> {
        let root: &tempfile::TempDir = &tempfile::tempdir()?;
        let root_path = root.path();
        let rootd = openat::Dir::open(root_path)?;
        let stripped_config = root_path.join("stripped");
        let content = r"
### BEGIN /etc/grub.d/10_linux ###

### END /etc/grub.d/10_linux ###

### BEGIN /etc/grub.d/15_ostree ###
menuentry 'Red Hat Enterprise Linux CoreOS 4 (ostree)' --class gnu-linux --class gnu --class os --unrestricted 'ostree-0-a92522f9-74dc-456a-ae0c-05ba22bca976' {
load_video
set gfxpayload=keep
insmod gzio
insmod part_gpt
insmod ext2
if [ x$feature_platform_search_hint = xy ]; then
  search --no-floppy --fs-uuid --set=root  a92522f9-74dc-456a-ae0c-05ba22bca976
else
  search --no-floppy --fs-uuid --set=root a92522f9-74dc-456a-ae0c-05ba22bca976
fi
linuxefi /ostree/rhcos-bf3b382/vmlinuz console=tty0 console=ttyS0,115200n8 rootflags=defaults,prjquota rw $ignition_firstboot root=UUID=cbac8cdc
initrdefi /ostree/rhcos-bf3b382/initramfs.img
}
### END /etc/grub.d/15_ostree ###

### BEGIN /etc/grub.d/20_linux_xen ###
### END /etc/grub.d/20_linux_xen ###";

        strip_grub_config_file(
            BufReader::new(std::io::Cursor::new(content)),
            &rootd,
            stripped_config.to_str().unwrap(),
        )?;
        let stripped_content = fs::read_to_string(stripped_config)?;
        let expected = r"
### BEGIN /etc/grub.d/10_linux ###

### END /etc/grub.d/10_linux ###


### BEGIN /etc/grub.d/20_linux_xen ###
### END /etc/grub.d/20_linux_xen ###
";
        assert_eq!(expected, stripped_content);
        Ok(())
    }
}
