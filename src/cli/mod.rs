//! Command-line interface (CLI) logic.

use anyhow::Result;
use clap::Parser;
use log::LevelFilter;
mod bootupctl;
pub(crate) mod bootupd;

/// Top-level multicall CLI.
#[derive(Debug, Parser)]
pub enum MultiCall {
    Ctl(bootupctl::CtlCommand),
    D(bootupd::DCommand),
}

impl MultiCall {
    pub fn from_args(args: Vec<String>) -> Self {
        use std::os::unix::ffi::OsStrExt;

        // This is a multicall binary, dispatched based on the introspected
        // filename found in argv[0].
        let exe_name = {
            let arg0 = args.get(0).cloned().unwrap_or_default();
            let exe_path = std::path::PathBuf::from(arg0);
            exe_path.file_name().unwrap_or_default().to_os_string()
        };
        #[allow(clippy::wildcard_in_or_patterns)]
        match exe_name.as_bytes() {
            b"bootupctl" => MultiCall::Ctl(bootupctl::CtlCommand::parse_from(args)),
            b"bootupd" | _ => MultiCall::D(bootupd::DCommand::parse_from(args)),
        }
    }

    pub fn run(self) -> Result<()> {
        match self {
            MultiCall::Ctl(ctl_cmd) => ctl_cmd.run(),
            MultiCall::D(d_cmd) => d_cmd.run(),
        }
    }

    /// Return the log-level set via command-line flags.
    pub fn loglevel(&self) -> LevelFilter {
        match self {
            MultiCall::Ctl(cmd) => cmd.loglevel(),
            MultiCall::D(cmd) => cmd.loglevel(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// set-default-bootloader records the default in the components that have
    /// update metadata, as on an image without BIOS.json.
    #[test]
    #[cfg(efi_arch)]
    fn test_set_default_bootloader() -> Result<()> {
        use crate::bootloader::Bootloader;
        use crate::component::write_update_metadata;
        use crate::model::ContentMetadata;
        let td = tempfile::tempdir()?;
        let root = td.path().to_str().unwrap();
        let opts = || bootupd::DefaultBootloaderOpts {
            sysroot: Some(root.to_string()),
            bootloader: Bootloader::Grub,
        };
        let no_efi = "Cannot set the default bootloader: there is no EFI update metadata";
        let err = bootupd::DCommand::set_default_bootloader(opts()).unwrap_err();
        assert!(err.to_string().starts_with(no_efi), "{err}");

        // BIOS metadata alone is not enough, as `install` only reads EFI's default
        std::fs::create_dir_all(td.path().join(crate::model::BOOTUPD_UPDATES_DIR))?;
        let meta: ContentMetadata = serde_json::from_str(
            r#"{"timestamp":"2026-11-16T17:14:09Z","version":"grub2 1:2.12-58.fc45,shim 16.1-7","versions":[{"name":"grub2","evr":"1:2.12-58.fc45"},{"name":"shim","evr":"16.1-7"}]}"#,
        )?;
        write_update_metadata(root, "BIOS.json".into(), &meta)?;
        let err = bootupd::DCommand::set_default_bootloader(opts()).unwrap_err();
        assert!(err.to_string().starts_with(no_efi), "{err}");

        let root_dir = cap_std_ext::cap_std::fs::Dir::open_ambient_dir(
            root,
            cap_std_ext::cap_std::ambient_authority(),
        )?;
        let efi = crate::efi::Efi::default();
        let efi_default = || crate::component::Component::get_default_bootloader(&efi, &root_dir);

        // EFI metadata alone is enough, as on images without BIOS.json
        std::fs::remove_file(
            td.path()
                .join(crate::model::BOOTUPD_UPDATES_DIR)
                .join("BIOS.json"),
        )?;
        write_update_metadata(root, "EFI.json".into(), &meta)?;
        bootupd::DCommand::set_default_bootloader(opts())?;
        assert_eq!(efi_default()?, Some(Bootloader::Grub));

        // and so is metadata for both
        write_update_metadata(root, "EFI.json".into(), &meta)?;
        assert_eq!(efi_default()?, None);
        write_update_metadata(root, "BIOS.json".into(), &meta)?;
        bootupd::DCommand::set_default_bootloader(opts())?;
        assert_eq!(efi_default()?, Some(Bootloader::Grub));
        Ok(())
    }

    #[test]
    fn clap_apps() {
        use clap::CommandFactory;
        bootupctl::CtlCommand::command().debug_assert();
        bootupd::DCommand::command().debug_assert();
    }

    #[test]
    fn test_multicall_dispatch() {
        {
            let d_argv = vec![
                "/usr/bin/bootupd".to_string(),
                "generate-update-metadata".to_string(),
            ];
            let cli = MultiCall::from_args(d_argv);
            match cli {
                MultiCall::Ctl(cmd) => panic!("{:?}", cmd),
                MultiCall::D(_) => {}
            };
        }
        {
            let ctl_argv = vec!["/usr/bin/bootupctl".to_string(), "validate".to_string()];
            let cli = MultiCall::from_args(ctl_argv);
            match cli {
                MultiCall::Ctl(_) => {}
                MultiCall::D(cmd) => panic!("{:?}", cmd),
            };
        }
        {
            let ctl_argv = vec!["/bin-mount/bootupctl".to_string(), "validate".to_string()];
            let cli = MultiCall::from_args(ctl_argv);
            match cli {
                MultiCall::Ctl(_) => {}
                MultiCall::D(cmd) => panic!("{:?}", cmd),
            };
        }
    }

    #[test]
    fn test_verbosity() {
        let default = MultiCall::from_args(vec![
            "bootupd".to_string(),
            "generate-update-metadata".to_string(),
        ]);
        assert_eq!(default.loglevel(), LevelFilter::Warn);

        let info = MultiCall::from_args(vec![
            "bootupd".to_string(),
            "generate-update-metadata".to_string(),
            "-v".to_string(),
        ]);
        assert_eq!(info.loglevel(), LevelFilter::Info);
    }
}
