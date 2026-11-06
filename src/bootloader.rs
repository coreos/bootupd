use anyhow::Result;
use fn_error_context::context;
use serde::{Deserialize, Serialize};
use std::fmt::Display;

#[derive(Debug, Copy, Clone, clap::ValueEnum, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Bootloader {
    Grub,
    #[cfg(efi_arch)]
    GrubCC,
    #[cfg(efi_arch)]
    Systemd,
}

impl Display for Bootloader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Bootloader::Grub => f.write_str("grub"),
            #[cfg(efi_arch)]
            Bootloader::GrubCC => f.write_str("grub-cc"),
            #[cfg(efi_arch)]
            Bootloader::Systemd => f.write_str("systemd"),
        }
    }
}

impl Bootloader {
    #[cfg(efi_arch)]
    fn next(self) -> Option<Self> {
        match self {
            Self::Grub => Some(Self::GrubCC),
            Self::GrubCC => Some(Self::Systemd),
            Self::Systemd => None,
        }
    }

    #[cfg(not(efi_arch))]
    fn next(self) -> Option<Self> {
        match self {
            Self::Grub => None,
        }
    }

    pub(crate) fn iter() -> impl Iterator<Item = Self> {
        std::iter::successors(Some(Self::Grub), |v| v.next())
    }

    /// Returns the name of the EFI component for this particular bootloader
    /// We use directories inside /usr/lib/efi as values of EFI component
    ///
    /// Example
    /// /usr/lib/efi/
    /// |-- grub-cc
    /// |-- grub2
    /// `-- shim
    pub(crate) fn efi_component_name(&self) -> &'static str {
        match self {
            Bootloader::Grub => "grub2",
            #[cfg(efi_arch)]
            Bootloader::GrubCC => "grub-cc",
            #[cfg(efi_arch)]
            Bootloader::Systemd => "systemd-boot",
        }
    }

    #[cfg(efi_arch)]
    pub(crate) fn try_from_efi_component_name(component_name: &str) -> Result<Self> {
        match component_name {
            "grub2" => Ok(Self::Grub),
            #[cfg(efi_arch)]
            "grub-cc" => Ok(Self::GrubCC),
            #[cfg(efi_arch)]
            "systemd-boot" => Ok(Self::Systemd),
            _ => anyhow::bail!("Not a valid bootloader: {component_name}"),
        }
    }

    /// Returns the efi component names which do not match this bootloader.
    /// Used to skip other bootloaders' directories when building a component's [`FileTree`].
    pub(crate) fn dirs_to_skip(&self) -> Vec<&'static str> {
        Bootloader::iter()
            .filter(|b| b != self)
            .map(|b| b.efi_component_name())
            .collect()
    }
}

#[cfg(not(efi_arch))]
#[context("Getting bootloader")]
pub(crate) fn get_bootloader() -> Result<Bootloader> {
    Ok(Bootloader::Grub)
}

#[cfg(efi_arch)]
#[context("Getting bootloader")]
pub(crate) fn get_bootloader() -> Result<Bootloader> {
    use crate::efi::get_loader_info;
    use std::sync::OnceLock;

    static BOOTLOADER: OnceLock<Bootloader> = OnceLock::new();

    if let Some(bootloader) = BOOTLOADER.get() {
        return Ok(*bootloader);
    }

    let bootloader = match get_loader_info() {
        Some(info) => match info.to_lowercase() {
            i if i.contains("grub cc") => Bootloader::GrubCC,
            i if i.contains("systemd-boot") => Bootloader::Systemd,
            _ => Bootloader::Grub,
        },
        None => Bootloader::Grub,
    };

    BOOTLOADER.get_or_init(|| bootloader);

    return Ok(bootloader);
}
