use crate::bootupd::{self, ConfigMode};
use anyhow::{Context, Result};
use camino::Utf8Path;
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
}

#[derive(Debug, Parser)]
pub struct InstallOpts {
    /// Source root
    #[clap(long, value_parser, default_value_t = String::from("/"))]
    src_root: String,
    /// Target root
    #[clap(value_parser)]
    dest_root: String,

    /// Target device(s) for bootloader installation. Can be specified multiple
    /// times to install to multiple devices (e.g., for multi-disk RAID/LVM setups).
    #[clap(long, action = clap::ArgAction::Append, conflicts_with = "filesystem")]
    device: Vec<String>,

    /// Filesystem path to inspect for backing devices. Bootupd will walk up the
    /// device hierarchy to find physical disks and install to all ESPs found.
    #[clap(long)]
    filesystem: Option<String>,

    /// Enable installation of the built-in static config files
    #[clap(long)]
    with_static_configs: bool,

    /// Implies `--with-static-configs`.  When present, this also writes a
    /// file with the UUID of the target filesystems.
    #[clap(long)]
    write_uuid: bool,

    /// On EFI systems, invoke `efibootmgr` to update the firmware.
    #[clap(long)]
    update_firmware: bool,

    #[clap(long = "component", conflicts_with = "auto")]
    /// Only install these components
    components: Option<Vec<String>>,

    /// Automatically choose components based on booted host state.
    ///
    /// For example on x86_64, if the host system is booted via EFI,
    /// then only enable installation to the ESP.
    #[clap(long)]
    auto: bool,
}

#[derive(Debug, Parser)]
pub struct GenerateOpts {
    /// Physical root mountpoint
    #[clap(value_parser)]
    sysroot: Option<String>,
}

impl DCommand {
    /// Run CLI application.
    pub fn run(self) -> Result<()> {
        match self.cmd {
            DVerb::Install(opts) => Self::run_install(opts),
            DVerb::GenerateUpdateMetadata(opts) => Self::run_generate_meta(opts),
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
            let device = bootc_internal_blockdev::list_dev(Utf8Path::new(fs_path))?;
            device.find_all_roots()?
        } else {
            opts.device
                .iter()
                .map(|d| bootc_internal_blockdev::list_dev(Utf8Path::new(d)))
                .collect::<Result<Vec<_>>>()?
        };

        bootupd::install(
            &opts.src_root,
            &opts.dest_root,
            &devices,
            configmode,
            opts.update_firmware,
            opts.components.as_deref(),
            opts.auto,
        )
        .context("boot data installation failed")?;
        Ok(())
    }
}
