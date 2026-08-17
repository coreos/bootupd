use anyhow::{anyhow, Context};
use std::{
    os::{
        fd::IntoRawFd,
        unix::io::{FromRawFd, OwnedFd},
    },
    path::PathBuf,
};

#[cfg(efi_arch)]
mod sync_esps;

const SOCKET_PATH: &str = "/run/bootupd/org.coreos.bootupd1";

#[derive(Debug, Clone, zlink::ReplyError, zlink::introspect::ReplyError)]
#[zlink(interface = "org.coreos.bootupd1")]
enum BootupdVarlinkError {
    Failed { message: String },
    // Unfortunately we can't just remove endpoints based on architecture,
    // so this is a generic error to be returned when an endpoint is not
    // implemented for the target arch.
    MethodNotAvailableOnArch { message: String },
}

impl BootupdVarlinkError {
    fn new(message: String) -> Self {
        Self::Failed { message }
    }

    #[allow(dead_code)]
    fn not_available_on_arch() -> Self {
        Self::MethodNotAvailableOnArch {
            message: format!("This method is not available on {}", std::env::consts::ARCH),
        }
    }
}

impl From<anyhow::Error> for BootupdVarlinkError {
    fn from(err: anyhow::Error) -> Self {
        log::error!("varlink call failed: {err:#}");
        Self::Failed {
            message: format!("{err:#}"),
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
        #[cfg(efi_arch)]
        return sync_esps::_sync_fwupd_updates(partuuid, capsule_dir);
        #[cfg(not(efi_arch))]
        return Err(BootupdVarlinkError::not_available_on_arch());
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
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(async {
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
                zlink::tokio::unix::Listener::try_from(owned_fd)
                    .context("creating listener from socket-activated fd")?
            } else {
                // Bind our own socket
                let socket_path = get_socket()?;
                zlink::tokio::unix::bind(socket_path)?
            };

            let server = zlink::Server::new(listener, BootupdVarlinkService);
            server.run().await.context("running varlink service")
        })
}
