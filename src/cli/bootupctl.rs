use crate::ipc::ClientToDaemonConnection;
use anyhow::Result;
use log::LevelFilter;
use structopt::clap::AppSettings;
use structopt::StructOpt;

/// `bootupctl` sub-commands.
#[derive(Debug, StructOpt)]
#[structopt(name = "bootupctl", about = "Bootupd client application")]
pub struct CtlCommand {
    /// Verbosity level (higher is more verbose).
    #[structopt(short = "v", parse(from_occurrences), global = true)]
    verbosity: u8,

    /// CLI sub-command.
    #[structopt(subcommand)]
    pub cmd: CtlVerb,
}

impl CtlCommand {
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
#[derive(Debug, StructOpt)]
pub enum CtlVerb {
    // FIXME(lucab): drop this after refreshing
    // https://github.com/coreos/fedora-coreos-config/pull/595
    #[structopt(name = "backend", setting = AppSettings::Hidden)]
    Backend(CtlBackend),
    #[structopt(name = "status", about = "Show components status")]
    Status(StatusOpts),
    #[structopt(name = "update", about = "Update all components")]
    Update,
    #[structopt(name = "validate", about = "Validate system state")]
    Validate,
}

#[derive(Debug, StructOpt)]
pub enum CtlBackend {
    #[structopt(name = "generate-update-metadata", setting = AppSettings::Hidden)]
    Generate(super::bootupd::GenerateOpts),
    #[structopt(name = "install", setting = AppSettings::Hidden)]
    Install(super::bootupd::InstallOpts),
}

#[derive(Debug, StructOpt)]
pub struct StatusOpts {
    // Output JSON
    #[structopt(long)]
    json: bool,
}

impl CtlCommand {
    /// Run CLI application.
    pub fn run(self) -> Result<()> {
        match self.cmd {
            CtlVerb::Status(opts) => Self::run_status(opts),
            CtlVerb::Update => Self::run_update(),
            CtlVerb::Validate => Self::run_validate(),
            CtlVerb::Backend(CtlBackend::Generate(opts)) => {
                super::bootupd::DCommand::run_generate_meta(opts)
            }
            CtlVerb::Backend(CtlBackend::Install(opts)) => {
                super::bootupd::DCommand::run_install(opts)
            }
        }
    }

    /// Runner for `status` verb.
    fn run_status(opts: StatusOpts) -> Result<()> {
        let mut client = ClientToDaemonConnection::new();
        client.connect()?;

        let r: crate::Status = client.send(&crate::ClientRequest::Status)?;
        if opts.json {
            let stdout = std::io::stdout();
            let mut stdout = stdout.lock();
            serde_json::to_writer_pretty(&mut stdout, &r)?;
        } else {
            crate::print_status(&r);
        }

        client.shutdown()?;
        Ok(())
    }

    /// Runner for `update` verb.
    fn run_update() -> Result<()> {
        let mut client = ClientToDaemonConnection::new();
        client.connect()?;

        crate::client_run_update(&mut client)?;

        client.shutdown()?;
        Ok(())
    }

    /// Runner for `validate` verb.
    fn run_validate() -> Result<()> {
        let mut client = ClientToDaemonConnection::new();
        client.connect()?;
        crate::client_run_validate(&mut client)?;
        client.shutdown()?;
        Ok(())
    }
}
