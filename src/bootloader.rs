use anyhow::Result;
use fn_error_context::context;
use std::{fmt::Display, sync::OnceLock};

use crate::efi::get_loader_info;

#[derive(Debug, Default, Copy, Clone, clap::ValueEnum, PartialEq, Eq)]
pub enum Bootloader {
    #[default]
    Grub,
    GrubCC,
}

impl Display for Bootloader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Bootloader::Grub => f.write_str("grub"),
            Bootloader::GrubCC => f.write_str("grub-cc"),
        }
    }
}

impl Bootloader {
    fn next(self) -> Option<Self> {
        match self {
            Self::Grub => Some(Self::GrubCC),
            Self::GrubCC => None,
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
            Bootloader::GrubCC => "grub-cc",
        }
    }
}

#[context("Getting bootloader")]
pub(crate) fn get_bootloader() -> Result<Bootloader> {
    static BOOTLOADER: OnceLock<Bootloader> = OnceLock::new();

    if let Some(bootloader) = BOOTLOADER.get() {
        return Ok(*bootloader);
    }

    let bootloader = match get_loader_info() {
        Some(info) => {
            if info.to_lowercase().contains("grub cc") {
                Bootloader::GrubCC
            } else {
                Bootloader::Grub
            }
        }
        None => Bootloader::Grub,
    };

    BOOTLOADER.get_or_init(|| bootloader);

    return Ok(bootloader);
}
