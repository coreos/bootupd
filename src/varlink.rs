use anyhow::{anyhow, Context};
use cap_std::{ambient_authority, fs::Dir};
use cap_std_ext::dirext::CapStdExtDirExt;
use log::info;
use std::{
    fs::create_dir_all,
    os::{
        fd::IntoRawFd,
        unix::io::{FromRawFd, OwnedFd},
    },
    path::{Path, PathBuf},
};

use crate::{
    bootupd::list_dev_current_root, efi, freezethaw::fsfreeze_thaw_cycle, model::SavedState,
};

const SOCKET_PATH: &str = "/run/bootupd/org.coreos.bootupd1";

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

#[derive(Debug, Clone, zlink::ReplyError, zlink::introspect::ReplyError)]
#[zlink(interface = "org.coreos.bootupd1")]
enum BootupdVarlinkError {
    Failed { message: String },
}

impl BootupdVarlinkError {
    fn new(message: String) -> Self {
        Self::Failed { message }
    }
}

impl From<anyhow::Error> for BootupdVarlinkError {
    fn from(err: anyhow::Error) -> Self {
        log::error!("varlink call failed: {err:#}");
        Self::Failed {
            message: format!("{err}"),
        }
    }
}

struct BootupdVarlinkService;

#[zlink::service(interface = "org.coreos.bootupd1")]
impl BootupdVarlinkService {
    /// Sync capsule update files from a "primary" ESP to all colocated ESPs.
    ///
    /// partuuid: GPT partition UUID of the ESP containing the source capsule files.
    /// capsule_dir: Path to the directory containing the source capsule files, relative
    ///              to the ESP root (e.g. "EFI/fedora/fw")
    #[allow(clippy::unused_async)]
    async fn sync_fwupd_updates(
        &mut self,
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
        if !src_capsule_path.is_dir() {
            drop(primary_efi);
            return Err(BootupdVarlinkError::new(format!(
                "Capsule directory not found at: {src_capsule_path:?}"
            )));
        }

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
                let contents = src_dir
                    .read(&name)
                    .with_context(|| format!("reading {name:?}"))?;
                dest_dir
                    .atomic_write(&name, &contents)
                    .with_context(|| format!("writing {name:?}"))?;
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
}

/// Ensure the Unix socket can be created
fn get_socket() -> anyhow::Result<PathBuf> {
    let socket_path = PathBuf::from(SOCKET_PATH);

    // Ensure the parent directory exists.
    if let Some(parent) = socket_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating directory {}", parent.display()))?;
    }

    // Remove any stale socket from a previous run.
    if let Err(e) = std::fs::remove_file(&socket_path) {
        if e.kind() != std::io::ErrorKind::NotFound {
            return Err(e)
                .with_context(|| format!("removing stale socket {}", socket_path.display()));
        }
    }

    Ok(socket_path)
}

pub fn run_varlink_service() -> anyhow::Result<()> {
    smol::block_on(async {
        let listener = if std::env::var_os("LISTEN_FDS").is_some() {
            // Socket-activated
            let fd = libsystemd::activation::receive_descriptors(false)
                .context("receiving socket-activated fds")?
                .into_iter()
                .next()
                .ok_or_else(|| anyhow!("no fds received"))?;
            // SAFETY: `into_raw_fd` transfers ownership from `FileDescriptor`, ensuring the
            // fd is valid and not closed elsewhere. `from_raw_fd` takes exclusive ownership.
            let owned_fd = unsafe { OwnedFd::from_raw_fd(fd.into_raw_fd()) };
            zlink::smol::unix::Listener::try_from(owned_fd)
                .context("creating listener from socket-activated fd")?
        } else {
            // Bind our own socket
            let socket_path = get_socket()?;
            zlink::smol::unix::bind(socket_path)?
        };

        let server = zlink::Server::new(listener, BootupdVarlinkService);
        server.run().await.context("running varlink service")
    })
}
