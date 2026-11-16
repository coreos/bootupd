use crate::aleph;
#[cfg(any(target_arch = "x86_64", target_arch = "powerpc64"))]
use crate::bios;
use crate::bootloader::{get_bootloader, Bootloader};
use crate::cli::bootupd::InstallOpts;
use crate::component::{self, ComponentType};
use crate::component::{Component, ValidationResult};
#[cfg(any(
    target_arch = "x86_64",
    target_arch = "aarch64",
    target_arch = "riscv64"
))]
use crate::efi;
use crate::freezethaw::fsfreeze_thaw_cycle;
#[cfg(any(
    target_arch = "x86_64",
    target_arch = "aarch64",
    target_arch = "powerpc64",
    target_arch = "riscv64"
))]
use crate::grubconfigs::{ensure_grub_permissions, GRUB2DIR};
use crate::model::{ComponentStatus, ComponentUpdatable, ContentMetadata, SavedState, Status};
use crate::{ostreeutil, util};
use anyhow::{anyhow, Context, Result};
use camino::{Utf8Path, Utf8PathBuf};
use cap_std_ext::dirext::CapStdExtDirExt;
use clap::crate_version;
use fn_error_context::context;
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use bootc_internal_blockdev::Device;
use cap_std::ambient_authority;
use cap_std::fs::{Dir, Permissions, PermissionsExt};
use cap_std_ext::cap_std;

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

/// Choose the EFI bootloader to install when neither `--bootloader` nor a
/// default bootloader says which, from the update payload in `src_root` and
/// its metadata `meta`, see [`pick_efi_bootloader`].
#[cfg(efi_arch)]
fn choose_efi_bootloader(src_root: &Utf8Path, meta: &ContentMetadata) -> Result<Bootloader> {
    let carried = crate::efi::bootloaders_in_payload(src_root)?;
    let location = crate::efi::payload_location(src_root)?;
    pick_efi_bootloader(&carried, &meta.available_bootloaders(), &location)
}

/// Picks the single bootloader that the EFI update payload carries (`carried`)
/// and its metadata names (`named`). An empty `named` does not narrow the
/// choice. `location` is only for error messages.
///
/// Narrowing the payload's bootloaders to what the metadata names keeps the
/// choice for metadata that was not regenerated after adding a component.
/// Metadata that bootupd 0.4.0 or newer generated for an image without
/// usr/lib/efi names packages such as grub2-efi-x64, and so no bootloader.
#[cfg(efi_arch)]
fn pick_efi_bootloader(
    carried: &[Bootloader],
    named: &[Bootloader],
    location: &Path,
) -> Result<Bootloader> {
    use crate::efi::bootloader_names;
    let undetermined = "Could not determine bootloader. Default bootloader not set";
    let location = location.display();
    let candidates = carried
        .iter()
        .copied()
        .filter(|b| named.is_empty() || named.contains(b))
        .collect::<Vec<_>>();
    match candidates.as_slice() {
        [bootloader] => Ok(*bootloader),
        [] if carried.is_empty() => {
            anyhow::bail!("{undetermined}. The EFI update payload in {location} carries no bootloader")
        }
        [] => anyhow::bail!(
            "{undetermined}. The EFI update payload in {location} carries {}, but its metadata names {}. Pass --bootloader, or regenerate the metadata",
            bootloader_names(carried),
            bootloader_names(named)
        ),
        candidates => anyhow::bail!(
            "{undetermined}. Multiple bootloaders found as install candidates: {}. Pass --bootloader, or set a default in the image with `bootupctl backend set-default-bootloader`",
            bootloader_names(candidates)
        ),
    }
}

/// The bootloader to install: the one given with `--bootloader`, or
/// else the EFI component's default, or GRUB when the BIOS component is among
/// the targets, or the only one the EFI payload offers, see
/// [`choose_efi_bootloader`]. Without `--bootloader`, a targeted EFI component
/// needs update metadata.
#[cfg(efi_arch)]
fn install_bootloader(
    opts: &InstallOpts,
    target_components: &[&dyn Component],
    source_root_dir: &Dir,
) -> Result<Bootloader> {
    Ok(match opts.bootloader {
        // CLI overrides anything else
        Some(b) => b,
        None => {
            let mut efi_default = None;
            let mut efi_component_update = None;
            let mut bios_default = None;

            for c in target_components {
                match c.component_type() {
                    ComponentType::Bios => {
                        bios_default = Some(Bootloader::Grub);
                    }
                    ComponentType::Efi => {
                        efi_default = c.get_default_bootloader(source_root_dir)?;

                        if efi_default.is_none() {
                            // We don't want to filter any bootloader
                            efi_component_update = c.get_component_update(source_root_dir, None)?;
                        }
                    }
                }
            }

            match (bios_default, efi_default) {
                // EFI bootloader takes precedence
                // Take the following example
                //  - BIOS default = Grub (always)
                //  - EFI default = GrubCC
                //
                //  We can't install GrubCC for BIOS as it's not supported
                //  So we just default to installing GrubCC
                (_, Some(eb)) => eb,
                (Some(bb), None) => bb,

                // We still can get the bootloader from the EFI update payload
                // If there's only ONE bootloader to choose from, then that's the
                // one to be installed
                (None, None) => {
                    let Some(efi_component_update) = efi_component_update else {
                        anyhow::bail!("Could not determine bootloader. Default bootloader not set")
                    };

                    choose_efi_bootloader(Utf8Path::new(&opts.src_root), &efi_component_update)?
                }
            }
        }
    })
}

/// The components an install with `components` (`--component`) targets, out
/// of `all_components` as [`get_components_impl`] returns them, with or
/// without `--auto`.
fn select_components<'a>(
    all_components: &'a Components,
    components: Option<&[String]>,
) -> Result<Vec<&'a dyn Component>> {
    let Some(components) = components else {
        return Ok(all_components.values().map(|c| c.as_ref()).collect());
    };
    components
        .iter()
        .map(|name| {
            all_components
                .get(name.as_str())
                .map(|c| c.as_ref())
                .ok_or_else(|| anyhow!("Unknown component: {name}"))
        })
        .collect()
}

/// The `targets` that `install` installs `bootloader` for, in order, after
/// checking each one's update payload in `src_root`, so that a payload one of
/// them refuses fails before any is installed. BIOS is skipped without
/// `devices`, and any component that does not support `bootloader` or has no
/// update metadata.
fn components_to_install<'a>(
    targets: &[&'a dyn Component],
    devices: &[Device],
    bootloader: Bootloader,
    src_root: &str,
    source_root_dir: &Dir,
) -> Result<Vec<&'a dyn Component>> {
    let mut components = Vec::new();
    for &component in targets {
        // skip for BIOS if no devices specified
        if component.component_type() == ComponentType::Bios && devices.is_empty() {
            println!(
                "Skip installing component {} without target device",
                component.name()
            );
            continue;
        }

        if !component.is_bootloader_supported(bootloader) {
            println!(
                "Skip installing component {} as it does not support bootloader {bootloader}",
                component.name()
            );
            continue;
        }

        // skip components that don't have an update metadata
        if component
            .query_update(source_root_dir, bootloader)?
            .is_none()
        {
            println!(
                "Skip installing component {} without update metadata",
                component.name()
            );
            continue;
        }
        components.push(component);
    }
    for component in &components {
        component
            .check_payload(src_root, bootloader)
            .with_context(|| format!("installing component {}", component.name()))?;
    }
    Ok(components)
}

pub(crate) fn install(opts: &InstallOpts, devices: &[Device], configs: ConfigMode) -> Result<()> {
    let all_components = get_components_impl(opts.auto)?;

    if all_components.is_empty() {
        println!("No components available for this platform.");
        return Ok(());
    }

    let source_root_dir = Dir::open_ambient_dir(&opts.src_root, ambient_authority())
        .context("Opening source root")?;

    // Checked by CLI parser
    assert!(opts.components.is_none() || !opts.auto);
    let target_components = select_components(&all_components, opts.components.as_deref())?;

    if target_components.is_empty() && !opts.auto {
        anyhow::bail!("No components specified");
    }

    log::debug!(
        "Auto: {}, Target components: {:?}",
        opts.auto,
        target_components
            .iter()
            .map(|c| c.name())
            .collect::<Vec<_>>()
    );

    let has_efi_component = target_components
        .iter()
        .any(|c| c.component_type() == ComponentType::Efi);

    // SavedState needs to be per component
    // Consider this scenario:
    // - Grub installed (statefile in /sysroot/boot)
    // - Re-install attempted with GrubCC
    //
    // So we can't just check statefile based on the determined bootloader
    // We need to check all cases
    for b in Bootloader::iter() {
        if !has_efi_component && b != Bootloader::Grub {
            log::debug!("Skipping bootloader {b} as there's no EFI component");
            continue;
        }

        SavedState::ensure_not_present(&opts.dest_root, b)
            .context("failed to install, invalid re-install attempted")?;
    }

    #[cfg(efi_arch)]
    let bootloader = install_bootloader(opts, &target_components, &source_root_dir)?;

    #[cfg(not(efi_arch))]
    let bootloader = Bootloader::Grub;

    let components = components_to_install(
        &target_components,
        devices,
        bootloader,
        &opts.src_root,
        &source_root_dir,
    )?;

    let mut state = SavedState::default();
    let mut installed_efi_vendor = None;

    for &component in components.iter() {
        // Determine which devices to install to. For EFI, filter to only
        // devices that have an ESP partition.
        let devices_to_install: Vec<Option<&Device>> = if devices.is_empty() {
            // No devices specified: install once with auto-detection (None).
            vec![None]
        } else if component.component_type() == ComponentType::Efi {
            // For EFI, only install to devices that have an ESP partition.
            let esp_devices: Vec<&Device> = devices
                .iter()
                .filter(|dev| match dev.find_partition_of_esp() {
                    Ok(_) => true,
                    Err(e) => {
                        log::warn!("Skipping device {} for EFI: {:#}", dev.path(), e);
                        false
                    }
                })
                .collect();
            if esp_devices.is_empty() {
                // No devices had a detectable ESP partition. This can happen with
                // loopback devices or other setups where partition scanning fails.
                // Fall back to auto-detection — the EFI component will look for
                // an already-mounted ESP at the destination.
                log::info!(
                    "No ESP partitions detected on specified devices; \
                     falling back to mounted ESP detection"
                );
                vec![None]
            } else {
                esp_devices.into_iter().map(Some).collect()
            }
        } else {
            devices.iter().map(|dev| Some(dev)).collect()
        };

        for device in &devices_to_install {
            let device_desc = device.map_or("(auto)".to_string(), |d| d.path());
            let meta = component
                .install(
                    &opts.src_root,
                    &opts.dest_root,
                    *device,
                    opts.update_firmware,
                    bootloader,
                )
                .with_context(|| {
                    format!(
                        "installing component {} to device {}",
                        component.name(),
                        device_desc,
                    )
                })?;
            log::info!(
                "Installed {} {} to {}",
                component.name(),
                meta.meta.version,
                device_desc,
            );
            // State is tracked per-component, not per-device. The metadata
            // (version, filetree, etc.) is identical across all devices, so we
            // only need to record it once. Updates and validation discover all
            // colocated devices at runtime (e.g. via find_colocated_esps()).
            if !state.installed.contains_key(component.name()) {
                state.installed.insert(component.name().into(), meta);
            }
            // Yes this is a hack...the Component thing just turns out to be too generic.
            if installed_efi_vendor.is_none() {
                if let Some(vendor) = component.get_efi_vendor(Path::new(&opts.src_root))? {
                    installed_efi_vendor = Some(vendor);
                }
            }
        }
    }
    let sysroot = &Dir::open_ambient_dir(&opts.dest_root, ambient_authority())?;

    #[cfg(any(
        target_arch = "x86_64",
        target_arch = "aarch64",
        target_arch = "powerpc64",
        target_arch = "riscv64"
    ))]
    if bootloader == Bootloader::Grub {
        match configs.enabled_with_uuid() {
            Some(uuid) => {
                let meta = get_static_config_meta()?;
                state.static_configs = Some(meta);

                let mut esp_dir = None;

                #[cfg(efi_arch)]
                {
                    for c in &target_components {
                        use crate::efi::Efi;

                        if let Some(efi) = c.as_any().downcast_ref::<Efi>() {
                            let mounted_esp_path = efi.get_esp_mountpoint();
                            let mounted_esp_path = mounted_esp_path.as_ref().ok_or_else(|| {
                                anyhow!("Mounted ESP not found to install static grub config")
                            })?;

                            esp_dir = Some(
                                Dir::open_ambient_dir(mounted_esp_path, ambient_authority())
                                    .context("Opening mounted ESP dir")?,
                            );

                            break;
                        }
                    }
                }

                crate::grubconfigs::install(
                    sysroot,
                    Some(&source_root_dir),
                    installed_efi_vendor.as_deref(),
                    uuid,
                    esp_dir.as_ref(),
                )?;
            }
            // On other architectures, assume that there's nothing to do.
            None => {}
        }
    }

    // Unmount the ESP, etc.
    drop(target_components);

    let mut state_guard = SavedState::unlocked(opts.dest_root.clone().into(), sysroot.try_clone()?)
        .context("failed to acquire write lock")?;
    state_guard
        .update_state(&state, bootloader)
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

        #[cfg(efi_arch)]
        default_bootloader: None,
    };
    Ok(self_meta)
}

type Components = BTreeMap<&'static str, Box<dyn Component>>;

/// Return the set of known components; if `auto` is specified then the system
/// filters to the target booted state, and fails if it cannot check for
/// `/sys/firmware/efi`.
#[cfg_attr(not(target_arch = "x86_64"), allow(unused_variables, unused_mut))]
pub(crate) fn get_components_impl(auto: bool) -> Result<Components> {
    let mut components = get_components();
    #[cfg(target_arch = "x86_64")]
    if auto {
        let booted = if crate::efi::is_efi_booted()? {
            ComponentType::Efi
        } else {
            ComponentType::Bios
        };
        log::info!("System boot method: {booted}");
        components.retain(|_, c| c.component_type() == booted);
    }
    Ok(components)
}

#[allow(clippy::box_default)]
/// Return the set of known components for this platform.
pub(crate) fn get_components() -> Components {
    #[cfg(not(target_arch = "s390x"))]
    let mut components = BTreeMap::new();

    #[cfg(target_arch = "s390x")]
    let components = BTreeMap::new();

    fn insert_component(components: &mut Components, component: Box<dyn Component>) {
        components.insert(component.name(), component);
    }

    #[cfg(target_arch = "x86_64")]
    {
        insert_component(&mut components, Box::new(bios::Bios::default()));
        insert_component(&mut components, Box::new(efi::Efi::default()));
    }
    #[cfg(any(target_arch = "aarch64", target_arch = "riscv64"))]
    insert_component(&mut components, Box::new(efi::Efi::default()));

    #[cfg(target_arch = "powerpc64")]
    insert_component(&mut components, Box::new(bios::Bios::default()));

    components
}

/// The bootloaders an update payload offers, as reported by
/// `bootupctl backend list-bootloaders`.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct PayloadBootloaders {
    /// The bootloaders `install --bootloader` can install, in
    /// [`Bootloader::iter`] order.
    pub(crate) available: Vec<Bootloader>,
    /// The EFI component's default bootloader, set with
    /// `set-default-bootloader`. `None` if unset or if the EFI component is not
    /// selected.
    pub(crate) default: Option<Bootloader>,
}

impl PayloadBootloaders {
    /// The JSON form, with the bootloader names `--bootloader` takes. Other
    /// tools parse it, so keys may be added, but existing ones keep their
    /// meaning.
    pub(crate) fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "bootloaders": self.available.iter().map(|b| b.to_string()).collect::<Vec<_>>(),
            "default-bootloader": self.default.map(|b| b.to_string()),
        })
    }

    /// The text form: one bootloader per line, with the default marked, even
    /// if it cannot be installed.
    pub(crate) fn to_text(&self) -> String {
        let mut text = self
            .available
            .iter()
            .map(|b| {
                if self.default == Some(*b) {
                    format!("{b} (default)\n")
                } else {
                    format!("{b}\n")
                }
            })
            .collect::<String>();
        if let Some(default) = self.default.filter(|b| !self.available.contains(b)) {
            text.push_str(&format!("{default} (default, cannot be installed)\n"));
        }
        text
    }
}

/// The bootloaders `install` can install from the update payload in
/// `src_root`, for an `install` with the same `auto` and `components`. Unlike
/// for `status`, `src_root` need not be booted. See README-devel.md and
/// [`combine_payload_bootloaders`] for how the selected components are
/// combined. A `src_root` or selected payload that cannot be read is an error,
/// even for a component `install` would skip.
#[context("Listing the bootloaders of the update payload")]
pub(crate) fn payload_bootloaders(
    src_root: &str,
    auto: bool,
    components: Option<&[String]>,
) -> Result<PayloadBootloaders> {
    let all_components = get_components_impl(auto)?;
    if all_components.is_empty() {
        // Like `install`, which then has nothing to install
        return Ok(PayloadBootloaders::default());
    }
    // Report a source root that cannot be opened with `install`'s error, not
    // a component's.
    Dir::open_ambient_dir(src_root, ambient_authority()).context("Opening source root")?;
    let mut payloads = Vec::new();
    for component in select_components(&all_components, components)? {
        let payload = component
            .payload_bootloaders(src_root)
            .with_context(|| format!("Reading the update payload of {}", component.name()))?;
        match payload {
            Some(payload) => payloads.push((component, payload)),
            None => log::warn!(
                "Component {} has no update metadata; install would skip it",
                component.name()
            ),
        }
    }
    Ok(combine_payload_bootloaders(&payloads))
}

/// Combine the bootloaders of the components with update metadata. A
/// bootloader is listed if every one of them that supports it can install it,
/// as `install` requires, and at least one of them can. Otherwise a bootloader
/// none of them supports would be listed.
fn combine_payload_bootloaders(
    payloads: &[(&dyn Component, PayloadBootloaders)],
) -> PayloadBootloaders {
    let available = Bootloader::iter()
        .filter(|&b| {
            payloads.iter().any(|(_, p)| p.available.contains(&b))
                && payloads
                    .iter()
                    .all(|(c, p)| !c.is_bootloader_supported(b) || p.available.contains(&b))
        })
        .collect();
    let default = payloads.iter().find_map(|(_, p)| p.default);
    PayloadBootloaders { available, default }
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
    let bootloader = get_bootloader()?;

    let mut state = SavedState::load_from_disk("/", Some(bootloader))?.unwrap_or_default();
    let component = component::new_from_name(name)?;
    let inst = if let Some(inst) = state.installed.get(name) {
        inst.clone()
    } else {
        anyhow::bail!("Component {} is not installed", name);
    };
    let sysroot = &rootcxt.sysroot;
    let (update, updatable) = component
        .query_updatable(rootcxt.as_sysroot(), &inst, bootloader)
        .with_context(|| format!("Failed to check {} for updates", name))?;
    let update = match (update, &updatable) {
        (Some(p), ComponentUpdatable::Upgradable) => p,
        (Some(_), ComponentUpdatable::NoUpdateAvailable) => {
            anyhow::bail!("update metadata exists but the component reports NoUpdateAvailable");
        }
        // No update metadata, only an error when the active boot method
        // requires it
        (None, _) => {
            log::debug!("Skipping update, updatable: {updatable:?}");
            component.query_requires_update(sysroot)?;
            return Ok(ComponentUpdateResult::AtLatestVersion);
        }
        // The content already matches. Another client completed this
        // update between our status query and this one.
        (Some(_), ComponentUpdatable::AtLatestVersion) => {
            log::debug!("Skipping update, updatable: {updatable:?}");
            return Ok(ComponentUpdateResult::AtLatestVersion);
        }
    };

    ensure_writable_boot()?;
    // Verify the permissions of grub files are 0600
    #[cfg(any(
        target_arch = "x86_64",
        target_arch = "aarch64",
        target_arch = "powerpc64",
        target_arch = "riscv64"
    ))]
    {
        if bootloader == Bootloader::Grub {
            let grub2dir = &sysroot
                .open_dir(format!("boot/{GRUB2DIR}"))
                .context("Opening /boot/grub2")?;
            ensure_grub_permissions(grub2dir)?;
        }
    }

    let mut pending_container = state.pending.take().unwrap_or_default();
    let interrupted = pending_container.get(component.name()).cloned();
    pending_container.insert(component.name().into(), update.clone());
    let sysroot = sysroot.try_clone()?;
    let mut state_guard = SavedState::acquire_write_lock(rootcxt.path.clone(), sysroot)
        .context("Failed to acquire write lock")?;
    state_guard
        .update_state(&state, bootloader)
        .context("Failed to update state")?;

    let newinst = component
        .run_update(rootcxt, &inst)
        .with_context(|| format!("Failed to update {}", component.name()))?;
    state.installed.insert(component.name().into(), newinst);
    pending_container.remove(component.name());
    state_guard.update_state(&state, bootloader)?;

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
    let bootloader = get_bootloader()?;
    let sysroot = &rootcxt.sysroot;
    let mut state = SavedState::load_from_disk("/", Some(bootloader))?.unwrap_or_default();
    let component = component::new_from_name(name)?;
    if state.installed.contains_key(name) {
        anyhow::bail!("Component {} is already installed", name);
    };

    ensure_writable_boot()?;
    // Verify the permissions of grub files are 0600
    #[cfg(any(
        target_arch = "x86_64",
        target_arch = "aarch64",
        target_arch = "powerpc64",
        target_arch = "riscv64"
    ))]
    {
        if bootloader == Bootloader::Grub {
            let grub2dir = &sysroot
                .open_dir(format!("boot/{GRUB2DIR}"))
                .context("Opening /boot/grub2")?;
            ensure_grub_permissions(grub2dir)?;
        }
    }

    let Some(update) = component.query_update(sysroot, bootloader)? else {
        anyhow::bail!("Component {} has no available update", name);
    };

    let sysroot = sysroot.try_clone()?;
    let mut state_guard = SavedState::acquire_write_lock(rootcxt.path.clone(), sysroot)
        .context("Failed to acquire write lock")?;

    let inst = component
        .adopt_update(&rootcxt, &update, with_static_config, bootloader)
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
        state_guard.update_state(&state, bootloader)?;
        return Ok(Some(update));
    } else {
        // Nothing adopted, skip
        log::info!("Component '{}' skipped adoption", component.name());
        return Ok(None);
    }
}

/// Get the block device backing the current root by trying `/boot` first,
/// then falling back to `/sysroot`. This avoids issues with virtual
/// filesystems like composefs that are mounted on `/`.
#[context("Finding block device from boot or sysroot")]
pub(crate) fn list_dev_current_root() -> Result<Device> {
    let auth = cap_std::ambient_authority();
    for path in ["/boot", "/sysroot"] {
        if let Ok(dir) = Dir::open_ambient_dir(path, auth) {
            if let Ok(dev) = bootc_internal_blockdev::list_dev_by_dir(&dir) {
                return Ok(dev);
            }
        }
    }
    anyhow::bail!("Failed to find block device from /boot or /sysroot")
}

/// daemon implementation of component validate
pub(crate) fn validate(name: &str) -> Result<ValidationResult> {
    let state = SavedState::load_from_disk("/", Some(get_bootloader()?))?.unwrap_or_default();
    let component = component::new_from_name(name)?;
    let Some(inst) = state.installed.get(name) else {
        anyhow::bail!("Component {} is not installed", name);
    };
    let device = list_dev_current_root()?;
    component.validate(inst, &device)
}

/// Impl for bootupctl status
/// This function assumes we're not running in a container
pub(crate) fn status() -> Result<Status> {
    let mut ret: Status = Default::default();
    let mut known_components = get_components();
    let path_root = Utf8Path::new("/");
    let root = Dir::open_ambient_dir(path_root, ambient_authority())?;

    // Populate aleph version
    ret.aleph_version =
        aleph::get_aleph_version(path_root.as_std_path())?.and_then(|a| a.aleph.version);

    // Populate boot method
    #[cfg(any(
        target_arch = "x86_64",
        target_arch = "aarch64",
        target_arch = "riscv64"
    ))]
    {
        ret.boot_method = Some(if efi::is_efi_booted()? { "EFI" } else { "BIOS" }.into());
    }

    let bootloader = get_bootloader()?;
    let state = SavedState::load_from_disk("/", Some(bootloader))?;

    if let Some(state) = state {
        let sysroot = Sysroot {
            dir: &root,
            path: path_root,
        };
        for (name, ic) in state.installed.iter() {
            log::trace!("Gathering status for installed component: {}", name);
            let component = known_components
                .remove(name.as_str())
                .ok_or_else(|| anyhow!("Unknown component installed: {}", name))?;
            let component = component.as_ref();
            let interrupted = state.pending.as_ref().and_then(|p| p.get(name.as_str()));
            let (update, updatable) = component
                .query_updatable(sysroot, &ic, bootloader)
                .with_context(|| format!("Querying update status for component {}", name))?;
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
        // such as a `version` in an aleph file (e.g. `.coreos-aleph-version.json` or `.bootc-aleph.json`),
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
            if component.query_update(&root, bootloader)?.is_none() {
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

    if let Some(aleph_version) = &status.aleph_version {
        println!("Aleph version: {}", aleph_version);
    }

    if let Some(boot_method) = &status.boot_method {
        println!("Boot method: {}", boot_method);
    }

    Ok(())
}

/// A borrowed view of a sysroot, used by the read-only query paths.
///
/// Unlike [`RootContext`], this carries no block device, so
/// callers like `status` avoid requiring device discovery.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Sysroot<'a> {
    pub(crate) dir: &'a Dir,
    pub(crate) path: &'a Utf8Path,
}

pub struct RootContext {
    pub sysroot: Dir,
    pub path: Utf8PathBuf,

    /// The block device backing the root filesystem.
    ///
    /// Used to determine the device to install to for components that need
    /// it, and passed to component update/adoption logic for validation.
    pub device: Device,
}

impl RootContext {
    fn new(sysroot: Dir, path: &str, device: Device) -> Self {
        Self {
            sysroot,
            path: Utf8Path::new(path).into(),
            device,
        }
    }

    /// The sysroot view used by the query paths, which do not touch the
    /// block device.
    pub(crate) fn as_sysroot(&self) -> Sysroot<'_> {
        Sysroot {
            dir: &self.sysroot,
            path: &self.path,
        }
    }
}

/// Initialize parent devices to prepare the update
fn prep_before_update() -> Result<RootContext> {
    let path = "/";
    let sysroot = Dir::open_ambient_dir(path, ambient_authority()).context("Opening root dir")?;
    let device = list_dev_current_root()?;
    Ok(RootContext::new(sysroot, path, device))
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
    let dirfd = Dir::open_ambient_dir(&grub_config_dir, ambient_authority())
        .context("Opening /boot/grub2")?;

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
                .rename(stripped_config, &dirfd, "grub.cfg")
                .context("Failed to replace symlink with current GRUB config")?;

            fsfreeze_thaw_cycle(dirfd.reopen_as_ownedfd()?)?;

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
    dirfd: &Dir,
    stripped_config_name: &str,
) -> Result<()> {
    let mut writer = vec![];

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

    // mode = -rw------- (600)
    dirfd
        .atomic_write_with_perms(stripped_config_name, writer, Permissions::from_mode(0o600))
        .with_context(|| format!("Writing to {stripped_config_name}"))?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(all(target_arch = "x86_64", efi_arch))]
    fn test_combine_payload_bootloaders() {
        use Bootloader::{Grub, GrubCC, Systemd};
        #[derive(Debug)]
        struct Case {
            /// What the BIOS payload can install, `None` without BIOS metadata
            bios: Option<&'static [Bootloader]>,
            /// What the EFI payload can install, `None` without EFI metadata
            efi: Option<&'static [Bootloader]>,
            default: Option<Bootloader>,
            expected: &'static [Bootloader],
        }
        let cases = [
            Case {
                bios: None,
                efi: None,
                default: None,
                expected: &[],
            },
            Case {
                bios: Some(&[Grub]),
                efi: None,
                default: None,
                expected: &[Grub],
            },
            Case {
                bios: None,
                efi: Some(&[Grub, Systemd]),
                default: None,
                expected: &[Grub, Systemd],
            },
            Case {
                bios: Some(&[Grub]),
                efi: Some(&[Grub, Systemd]),
                default: Some(Systemd),
                expected: &[Grub, Systemd],
            },
            // BIOS could install GRUB, but the EFI payload cannot.
            Case {
                bios: Some(&[Grub]),
                efi: Some(&[GrubCC]),
                default: None,
                expected: &[GrubCC],
            },
            Case {
                bios: Some(&[Grub]),
                efi: Some(&[]),
                default: None,
                expected: &[],
            },
            // The EFI payload could install GRUB, but BIOS cannot, e.g.
            // without grub2-install.
            Case {
                bios: Some(&[]),
                efi: Some(&[Grub, Systemd]),
                default: None,
                expected: &[Systemd],
            },
        ];
        let components = get_components();
        for case in &cases {
            let mut payloads = Vec::new();
            if let Some(available) = case.bios {
                let bios = PayloadBootloaders {
                    available: available.to_vec(),
                    default: None,
                };
                payloads.push((components["BIOS"].as_ref(), bios));
            }
            if let Some(available) = case.efi {
                let efi = PayloadBootloaders {
                    available: available.to_vec(),
                    default: case.default,
                };
                payloads.push((components["EFI"].as_ref(), efi));
            }
            let expected = PayloadBootloaders {
                available: case.expected.to_vec(),
                default: case.default,
            };
            assert_eq!(combine_payload_bootloaders(&payloads), expected, "{case:?}");
        }
    }

    /// With `auto`, only the components for how this host booted
    #[test]
    fn test_get_components_auto() -> Result<()> {
        let auto = get_components_impl(true)?;
        let names = auto.keys().copied().collect::<Vec<_>>();
        #[cfg(target_arch = "x86_64")]
        {
            let booted = if crate::efi::is_efi_booted()? {
                "EFI"
            } else {
                "BIOS"
            };
            assert_eq!(names, [booted]);
        }
        #[cfg(not(target_arch = "x86_64"))]
        assert_eq!(names, get_components().keys().copied().collect::<Vec<_>>());
        Ok(())
    }

    #[test]
    fn test_select_components() {
        let all = get_components();
        let names = |selected: Vec<&dyn Component>| -> Vec<&str> {
            selected.iter().map(|c| c.name()).collect()
        };
        let expected = all.keys().copied().collect::<Vec<_>>();
        assert_eq!(names(select_components(&all, None).unwrap()), expected);
        assert!(select_components(&all, Some(&[])).unwrap().is_empty());
        let unknown = ["foo".to_string()];
        let Err(err) = select_components(&all, Some(&unknown)) else {
            panic!("an unknown component must be refused");
        };
        assert_eq!(err.to_string(), "Unknown component: foo");
    }

    /// A whole EFI payload, as laid out on Fedora 45 with systemd-boot added,
    /// with and without shim
    #[test]
    #[cfg(efi_arch)]
    fn test_payload_bootloaders_on_root() -> Result<()> {
        use crate::efi::{BOOTLOADER_BIN, SHIM};
        use Bootloader::{Grub, Systemd};
        let components = get_components();
        let meta = |default_bootloader| ContentMetadata {
            timestamp: chrono::Utc::now(),
            version: "1.0-1".into(),
            versions: None,
            default_bootloader,
        };
        for with_shim in [true, false] {
            let td = tempfile::tempdir()?;
            let root = Utf8Path::from_path(td.path()).unwrap();
            std::fs::create_dir_all(root.join(crate::model::BOOTUPD_UPDATES_DIR))?;
            for name in ["grub2", "shim", "systemd-boot"] {
                let vendor = root.join("usr/lib/efi").join(name).join("1.0-1/EFI/fedora");
                std::fs::create_dir_all(&vendor)?;
                if name != "shim" {
                    std::fs::write(vendor.join(BOOTLOADER_BIN), "")?;
                } else if with_shim {
                    std::fs::write(vendor.join(SHIM), "")?;
                }
            }
            let data_name = components["EFI"].component_update_data_name();
            component::write_update_metadata(root.as_str(), data_name, &meta(Some(Systemd)))?;
            let available = if with_shim {
                vec![Grub, Systemd]
            } else {
                vec![]
            };
            let expected = PayloadBootloaders {
                available,
                default: Some(Systemd),
            };
            let efi = ["EFI".to_string()];
            let payload = payload_bootloaders(root.as_str(), false, Some(&efi))?;
            assert_eq!(payload, expected, "{with_shim}");
            // Without --component, the BIOS component has no update metadata
            // here and is skipped.
            assert_eq!(payload_bootloaders(root.as_str(), false, None)?, expected);
        }
        // A source root that cannot be opened is an error, as for `install`.
        let err = payload_bootloaders("/nonexistent/src-root", false, None).unwrap_err();
        assert!(
            format!("{err:#}").contains("Opening source root"),
            "{err:#}"
        );
        // So is a payload that cannot be read.
        let td = tempfile::tempdir()?;
        let root = Utf8Path::from_path(td.path()).unwrap();
        let updates = root.join(crate::model::BOOTUPD_UPDATES_DIR);
        std::fs::create_dir_all(&updates)?;
        let efi_json = components["EFI"].component_update_data_name();
        std::fs::write(updates.as_std_path().join(efi_json), "{garbage")?;
        let efi = ["EFI".to_string()];
        let err = payload_bootloaders(root.as_str(), false, Some(&efi)).unwrap_err();
        assert!(
            format!("{err:#}").contains("Reading the update payload of EFI"),
            "{err:#}"
        );
        Ok(())
    }

    #[test]
    #[cfg(efi_arch)]
    fn test_payload_bootloaders_output() {
        use Bootloader::{Grub, GrubCC, Systemd};
        // The bootloaders, and their JSON and text forms
        let cases = [
            (
                PayloadBootloaders::default(),
                r#"{"bootloaders":[],"default-bootloader":null}"#,
                "",
            ),
            (
                PayloadBootloaders {
                    available: vec![Grub, GrubCC, Systemd],
                    default: Some(Grub),
                },
                r#"{"bootloaders":["grub","grub-cc","systemd"],"default-bootloader":"grub"}"#,
                "grub (default)\ngrub-cc\nsystemd\n",
            ),
            // A default that cannot be installed
            (
                PayloadBootloaders {
                    available: vec![Grub],
                    default: Some(Systemd),
                },
                r#"{"bootloaders":["grub"],"default-bootloader":"systemd"}"#,
                "grub\nsystemd (default, cannot be installed)\n",
            ),
        ];
        for (bootloaders, json, text) in cases {
            assert_eq!(bootloaders.to_json().to_string(), json);
            assert_eq!(bootloaders.to_text(), text);
        }
    }

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
        let rootd = Dir::open_ambient_dir(root_path, ambient_authority())?;
        let stripped_config = "stripped";
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
            stripped_config,
        )?;
        let stripped_content = rootd.read_to_string(stripped_config)?;
        let expected = r"
### BEGIN /etc/grub.d/10_linux ###

### END /etc/grub.d/10_linux ###


### BEGIN /etc/grub.d/20_linux_xen ###
### END /etc/grub.d/20_linux_xen ###
";
        assert_eq!(expected, stripped_content);
        Ok(())
    }

    #[test]
    #[cfg(efi_arch)]
    fn test_pick_efi_bootloader() {
        use Bootloader::{Grub, GrubCC, Systemd};
        let location = Path::new("usr/lib/efi");
        // What the payload carries, what the metadata names, and the choice
        let cases: &[(&[Bootloader], &[Bootloader], Option<Bootloader>)] = &[
            // An image without usr/lib/efi, whose metadata names packages
            (&[Grub], &[], Some(Grub)),
            (&[Grub], &[Grub], Some(Grub)),
            (&[GrubCC], &[GrubCC], Some(GrubCC)),
            (&[Grub, Systemd], &[Grub, Systemd], None),
            // Metadata not regenerated after adding systemd-boot,
            (&[Grub, Systemd], &[Grub], Some(Grub)),
            // or after removing it
            (&[Grub], &[Grub, Systemd], Some(Grub)),
            (&[], &[], None),
            (&[Systemd], &[Grub], None),
        ];
        for (carried, named, expected) in cases {
            let chosen = pick_efi_bootloader(carried, named, location);
            assert_eq!(
                chosen.as_ref().ok(),
                expected.as_ref(),
                "{carried:?} {named:?}: {chosen:?}"
            );
        }
        let message = |carried: &[Bootloader], named: &[Bootloader]| {
            pick_efi_bootloader(carried, named, location)
                .unwrap_err()
                .to_string()
        };
        let several = message(&[Grub, Systemd], &[Grub, Systemd]);
        assert!(
            several.contains("candidates: grub, systemd. Pass --bootloader, or set a default"),
            "{several}"
        );
        let none = message(&[], &[]);
        assert!(
            none.ends_with("in usr/lib/efi carries no bootloader"),
            "{none}"
        );
        let other = message(&[Systemd], &[Grub]);
        assert!(
            other.contains("carries systemd, but its metadata names grub. Pass --bootloader"),
            "{other}"
        );
    }

    /// Metadata of an image without usr/lib/efi, as bootupd 0.4.0 writes it
    #[cfg(efi_arch)]
    const PACKAGE_METADATA: &str = r#"{"timestamp":"2026-11-16T17:14:09Z","version":"grub2-efi-x64 1:2.12-58.el10,shim-x64 16.1-1.el10","versions":[{"name":"grub2-efi-x64","evr":"1:2.12-58.el10"},{"name":"shim-x64","evr":"16.1-1.el10"}],"default-bootloader":null}"#;

    /// An empty file at `path` in `root`
    #[cfg(efi_arch)]
    fn write_empty(root: &Utf8Path, path: &str) -> Result<()> {
        let path = root.join(path);
        std::fs::create_dir_all(path.parent().unwrap())?;
        std::fs::write(path, "")?;
        Ok(())
    }

    /// The choice from a payload and its metadata as bootupd writes them
    #[test]
    #[cfg(efi_arch)]
    fn test_choose_efi_bootloader() -> Result<()> {
        use crate::efi::{BOOTLOADER_BIN, SHIM};
        let packages: ContentMetadata = serde_json::from_str(PACKAGE_METADATA)?;
        // As CentOS Stream 10's bootupd 0.2.35 wrote it, naming components
        let stream10: ContentMetadata = serde_json::from_str(
            r#"{"timestamp":"2026-10-19T21:00:56Z","version":"grub2-efi-x64-1:2.12-58.el10.x86_64,shim-x64-16.1-1.el10.x86_64","versions":[{"name":"grub2","rpm_evr":"1:2.12-58.el10"},{"name":"shim","rpm_evr":"16.1-1.el10"}]}"#,
        )?;
        // As written for usr/lib/efi before systemd-boot was added
        let components: ContentMetadata = serde_json::from_str(
            r#"{"timestamp":"2026-11-16T17:14:09Z","version":"grub2 1:2.12-58.fc45,shim 16.1-7","versions":[{"name":"grub2","evr":"1:2.12-58.fc45"},{"name":"shim","evr":"16.1-7"}],"default-bootloader":null}"#,
        )?;

        let td = tempfile::tempdir()?;
        let root = Utf8Path::from_path(td.path()).unwrap();
        write_empty(root, &format!("usr/lib/bootupd/updates/EFI/centos/{SHIM}"))?;
        let err = choose_efi_bootloader(root, &packages).unwrap_err();
        assert!(
            err.to_string()
                .ends_with("in usr/lib/bootupd/updates/EFI carries no bootloader"),
            "{err}"
        );
        write_empty(
            root,
            &format!("usr/lib/bootupd/updates/EFI/centos/{BOOTLOADER_BIN}"),
        )?;
        assert_eq!(choose_efi_bootloader(root, &packages)?, Bootloader::Grub);
        assert_eq!(choose_efi_bootloader(root, &stream10)?, Bootloader::Grub);

        let td = tempfile::tempdir()?;
        let root = Utf8Path::from_path(td.path()).unwrap();
        for (component, binary) in [
            ("grub2", BOOTLOADER_BIN),
            ("shim", SHIM),
            ("systemd-boot", BOOTLOADER_BIN),
        ] {
            write_empty(
                root,
                &format!("usr/lib/efi/{component}/1.0-1/EFI/fedora/{binary}"),
            )?;
        }
        assert_eq!(choose_efi_bootloader(root, &components)?, Bootloader::Grub);

        // Metadata naming two versions of grub2, which the payload lacks
        let two_grub2: ContentMetadata = serde_json::from_str(
            r#"{"timestamp":"2026-11-16T17:14:09Z","version":"grub2 1:2.12-56.fc44,grub2 1:2.12-58.fc44,shim 16.1-7","versions":[{"name":"grub2","evr":"1:2.12-56.fc44"},{"name":"grub2","evr":"1:2.12-58.fc44"},{"name":"shim","evr":"16.1-7"}],"default-bootloader":null}"#,
        )?;
        let td = tempfile::tempdir()?;
        let root = Utf8Path::from_path(td.path()).unwrap();
        for (component, binary) in [("shim", SHIM), ("systemd-boot", BOOTLOADER_BIN)] {
            write_empty(
                root,
                &format!("usr/lib/efi/{component}/1.0-1/EFI/fedora/{binary}"),
            )?;
        }
        let err = choose_efi_bootloader(root, &two_grub2).unwrap_err();
        assert!(
            err.to_string()
                .ends_with("carries systemd, but its metadata names grub. Pass --bootloader, or regenerate the metadata"),
            "{err}"
        );
        Ok(())
    }

    /// A payload a component refuses fails before any component is installed,
    /// even when BIOS, which comes first, could install.
    #[test]
    #[cfg(efi_arch)]
    fn test_components_to_install() -> Result<()> {
        use crate::efi::{BOOTLOADER_BIN, SHIM};
        let td = tempfile::tempdir()?;
        let root = Utf8Path::from_path(td.path()).unwrap();
        // The legacy update directory with shim, but without GRUB
        write_empty(root, &format!("usr/lib/bootupd/updates/EFI/centos/{SHIM}"))?;
        let updates = root.join("usr/lib/bootupd/updates");
        std::fs::write(updates.join("EFI.json"), PACKAGE_METADATA)?;
        std::fs::write(updates.join("BIOS.json"), PACKAGE_METADATA)?;
        let src_dir = Dir::open_ambient_dir(root, ambient_authority())?;
        let all = get_components();
        let targets = all.values().map(|c| c.as_ref()).collect::<Vec<_>>();
        let device: Device = serde_json::from_value(serde_json::json!({
            "name": "vda",
            "size": 0
        }))?;
        let devices = [device];
        let names = |components: Vec<&dyn Component>| {
            components.iter().map(|c| c.name()).collect::<Vec<_>>()
        };
        let install = |devices: &[Device]| {
            components_to_install(&targets, devices, Bootloader::Grub, root.as_str(), &src_dir)
        };

        let Err(err) = install(&devices) else {
            panic!("a payload EFI refuses must fail the install");
        };
        assert!(
            format!("{err:#}").starts_with(
                "installing component EFI: Cannot install bootloader 'grub': the update payload in usr/lib/bootupd/updates/EFI carries no bootloader"
            ),
            "{err:#}"
        );

        write_empty(
            root,
            &format!("usr/lib/bootupd/updates/EFI/centos/{BOOTLOADER_BIN}"),
        )?;
        let expected = if all.contains_key("BIOS") {
            vec!["BIOS", "EFI"]
        } else {
            vec!["EFI"]
        };
        assert_eq!(names(install(&devices)?), expected);
        // BIOS needs a device to install to
        assert_eq!(names(install(&[])?), ["EFI"]);
        // A component without update metadata is not checked either.
        std::fs::remove_file(updates.join("EFI.json"))?;
        std::fs::remove_file(updates.join(format!("EFI/centos/{BOOTLOADER_BIN}")))?;
        assert_eq!(
            names(install(&devices)?),
            expected
                .into_iter()
                .filter(|c| *c != "EFI")
                .collect::<Vec<_>>()
        );
        Ok(())
    }

    /// `install`'s choice, from its options, for the EFI component
    #[test]
    #[cfg(efi_arch)]
    fn test_install_bootloader() -> Result<()> {
        use crate::efi::{BOOTLOADER_BIN, SHIM};
        use clap::Parser;
        let td = tempfile::tempdir()?;
        let root = Utf8Path::from_path(td.path()).unwrap();
        for binary in [SHIM, BOOTLOADER_BIN] {
            write_empty(
                root,
                &format!("usr/lib/bootupd/updates/EFI/centos/{binary}"),
            )?;
        }
        std::fs::write(
            root.join("usr/lib/bootupd/updates/EFI.json"),
            PACKAGE_METADATA,
        )?;
        let src_dir = Dir::open_ambient_dir(root, ambient_authority())?;
        let components = get_components();
        let efi = components["EFI"].as_ref();
        let dest = tempfile::tempdir()?;
        let dest = Utf8Path::from_path(dest.path()).unwrap();
        let opts = |root: &Utf8Path, bootloader: &[&str]| {
            let args = ["install", "--src-root", root.as_str(), "--component", "EFI"];
            InstallOpts::try_parse_from(args.iter().chain(bootloader).chain([&dest.as_str()]))
        };
        assert_eq!(
            install_bootloader(&opts(root, &[])?, &[efi], &src_dir)?,
            Bootloader::Grub
        );
        // An explicit --bootloader wins, whether or not the payload carries it.
        let systemd = opts(root, &["--bootloader", "systemd"])?;
        assert_eq!(
            install_bootloader(&systemd, &[efi], &src_dir)?,
            Bootloader::Systemd
        );

        // A payload with two bootloaders needs a default, unless the BIOS
        // component is a target, which installs GRUB.
        let td = tempfile::tempdir()?;
        let root = Utf8Path::from_path(td.path()).unwrap();
        for (component, binary) in [
            ("grub2", BOOTLOADER_BIN),
            ("shim", SHIM),
            ("systemd-boot", BOOTLOADER_BIN),
        ] {
            write_empty(
                root,
                &format!("usr/lib/efi/{component}/1.0-1/EFI/fedora/{binary}"),
            )?;
        }
        let metadata = r#"{"timestamp":"2026-11-16T17:14:09Z","version":"grub2 1.0-1,shim 1.0-1,systemd-boot 1.0-1","versions":[{"name":"grub2","evr":"1.0-1"},{"name":"shim","evr":"1.0-1"},{"name":"systemd-boot","evr":"1.0-1"}],"default-bootloader":null}"#;
        let efi_json = root.join("usr/lib/bootupd/updates/EFI.json");
        std::fs::create_dir_all(efi_json.parent().unwrap())?;
        std::fs::write(&efi_json, metadata)?;
        let src_dir = Dir::open_ambient_dir(root, ambient_authority())?;
        let err = install_bootloader(&opts(root, &[])?, &[efi], &src_dir).unwrap_err();
        assert!(err.to_string().contains("Multiple bootloaders"), "{err}");
        let bios = components.get("BIOS").map(|c| c.as_ref());
        if let Some(bios) = bios {
            assert_eq!(
                install_bootloader(&opts(root, &[])?, &[bios, efi], &src_dir)?,
                Bootloader::Grub
            );
        }
        // The EFI default wins, even over the BIOS component.
        std::fs::write(
            &efi_json,
            metadata.replace(
                r#""default-bootloader":null"#,
                r#""default-bootloader":"Systemd""#,
            ),
        )?;
        assert_eq!(
            install_bootloader(&opts(root, &[])?, &[efi], &src_dir)?,
            Bootloader::Systemd
        );
        if let Some(bios) = bios {
            assert_eq!(
                install_bootloader(&opts(root, &[])?, &[bios, efi], &src_dir)?,
                Bootloader::Systemd
            );
        }
        Ok(())
    }
}
