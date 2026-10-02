use crate::bootloader::Bootloader;
use crate::bootupd::{self, ConfigMode};
#[cfg(efi_arch)]
use crate::varlink;
use anyhow::{Context, Result};
use camino::Utf8Path;
use cap_std::ambient_authority;
use cap_std::fs::Dir;
use cap_std_ext::cap_std;
use clap::Parser;
use log::LevelFilter;

/// `bootupd` sub-commands.
#[derive(Debug, Parser)]
#[clap(name = "bootupd", about = "Bootupd backend commands", version)]
pub struct DCommand {
    /// Verbosity level (higher is more verbose).
    #[clap(short = 'v', action = clap::ArgAction::Count, global = true)]
    verbosity: u8,

    /// CLI sub-command.
    #[clap(subcommand)]
    pub cmd: DVerb,
}

impl DCommand {
    /// Return the log-level set via command-line flags.
    pub(crate) fn loglevel(&self) -> LevelFilter {
        match self.verbosity {
            0 => LevelFilter::Warn,
            1 => LevelFilter::Info,
            2 => LevelFilter::Debug,
            _ => LevelFilter::Trace,
        }
    }
}

/// CLI sub-commands.
#[derive(Debug, Parser)]
pub enum DVerb {
    #[clap(name = "generate-update-metadata", about = "Generate metadata")]
    GenerateUpdateMetadata(GenerateOpts),
    #[clap(name = "install", about = "Install components")]
    Install(InstallOpts),
    #[cfg(efi_arch)]
    SetDefaultBootloader(DefaultBootloaderOpts),
    #[cfg(efi_arch)]
    #[clap(name = "varlink", hide = true, about = "Run the varlink service")]
    Varlink,
}

#[derive(Debug, Parser)]
pub(crate) struct InstallOpts {
    /// Source root
    #[clap(long, value_parser, default_value_t = String::from("/"))]
    pub(crate) src_root: String,
    /// Target root
    #[clap(value_parser)]
    pub(crate) dest_root: String,

    /// Target device(s) for bootloader installation. Can be specified multiple
    /// times to install to multiple devices (e.g., for multi-disk RAID/LVM setups).
    #[clap(long, action = clap::ArgAction::Append, conflicts_with = "filesystem")]
    pub(crate) device: Vec<String>,

    /// Filesystem path to inspect for backing devices. Bootupd will walk up the
    /// device hierarchy to find physical disks and install to all ESPs found.
    #[clap(long)]
    pub(crate) filesystem: Option<String>,

    /// Enable installation of the built-in static config files
    #[clap(long)]
    pub(crate) with_static_configs: bool,

    /// Implies `--with-static-configs`.  When present, this also writes a
    /// file with the UUID of the target filesystems.
    #[clap(long)]
    pub(crate) write_uuid: bool,

    /// On EFI systems, invoke `efibootmgr` to update the firmware.
    #[clap(long)]
    pub(crate) update_firmware: bool,

    #[clap(long = "component", conflicts_with = "auto")]
    /// Only install these components
    pub(crate) components: Option<Vec<String>>,

    /// Automatically choose components based on booted host state.
    ///
    /// For example on x86_64, if the host system is booted via EFI,
    /// then only enable installation to the ESP.
    #[clap(long)]
    pub(crate) auto: bool,

    /// The bootloader to use
    #[clap(long)]
    pub(crate) bootloader: Option<Bootloader>,
}

#[derive(Debug, Parser)]
pub struct GenerateOpts {
    /// Physical root mountpoint
    #[clap(value_parser)]
    sysroot: Option<String>,
}

#[derive(Debug, Parser)]
pub struct DefaultBootloaderOpts {
    /// Physical root mountpoint
    #[clap(long)]
    pub(crate) sysroot: Option<String>,
    /// The bootloader to be set as the default
    pub(crate) bootloader: Bootloader,
}

impl DCommand {
    /// Run CLI application.
    pub fn run(self) -> Result<()> {
        match self.cmd {
            DVerb::Install(opts) => Self::run_install(opts),
            DVerb::GenerateUpdateMetadata(opts) => Self::run_generate_meta(opts),
            #[cfg(efi_arch)]
            DVerb::SetDefaultBootloader(opts) => Self::set_default_bootloader(opts),
            #[cfg(efi_arch)]
            DVerb::Varlink => Self::run_varlink_service(),
        }
    }

    /// Runner for `generate-install-metadata` verb.
    pub(crate) fn run_generate_meta(opts: GenerateOpts) -> Result<()> {
        let sysroot = opts.sysroot.as_deref().unwrap_or("/");
        if sysroot != "/" {
            anyhow::bail!("Using a non-default sysroot is not supported: {}", sysroot);
        }
        bootupd::generate_update_metadata(sysroot).context("generating metadata failed")?;
        Ok(())
    }

    /// Runner for `install` verb.
    pub(crate) fn run_install(opts: InstallOpts) -> Result<()> {
        let configmode = if opts.write_uuid {
            ConfigMode::WithUUID
        } else if opts.with_static_configs {
            ConfigMode::Static
        } else {
            ConfigMode::None
        };

        // Resolve devices: either discover backing devices from a filesystem path,
        // or convert explicitly specified device paths to Device objects.
        let devices = if let Some(ref fs_path) = opts.filesystem {
            let dir = Dir::open_ambient_dir(fs_path, ambient_authority())
                .with_context(|| format!("Opening filesystem path {fs_path}"))?;
            let device = bootc_internal_blockdev::list_dev_by_dir(&dir)?;
            device.find_all_roots()?
        } else {
            opts.device
                .iter()
                .map(|d| bootc_internal_blockdev::list_dev(Utf8Path::new(d)))
                .collect::<Result<Vec<_>>>()?
        };

        bootupd::install(&opts, &devices, configmode).context("boot data installation failed")?;
        Ok(())
    }

    #[cfg(efi_arch)]
    pub(crate) fn set_default_bootloader(opts: DefaultBootloaderOpts) -> Result<()> {
        let all_components = crate::bootupd::get_components();
        let target_components: Vec<_> = all_components.values().collect();

        for &component in target_components.iter() {
            if !component.is_bootloader_supported(opts.bootloader) {
                log::info!(
                    "{} is not supported for {}. Skipping...",
                    opts.bootloader,
                    component.name()
                );

                continue;
            }

            component.set_default_bootloader(&opts)?;
        }

        Ok(())
    }

    #[cfg(efi_arch)]
    fn run_varlink_service() -> Result<()> {
        varlink::run_varlink_service()
    }
}
