//! On-disk saved state.

use crate::bootloader::Bootloader;
use crate::bootupd::list_dev_current_root;
use crate::efi::Efi;
use crate::freezethaw::fsfreeze_thaw_cycle;
use crate::model::SavedState;
use crate::util::SignalTerminationGuard;
use anyhow::{bail, Context, Result};
use bootc_internal_blockdev::Device;
use camino::Utf8PathBuf;
use cap_std::ambient_authority;
use cap_std::fs::{Dir, Permissions, PermissionsExt};
use cap_std_ext::dirext::CapStdExtDirExt;
use fn_error_context::context;
use fs2::FileExt;
use std::fs::File;
use std::io::prelude::*;
use std::path::Path;

fn parse_statefile(statusf: cap_std::fs::File) -> Result<Option<SavedState>> {
    let mut bufr = std::io::BufReader::new(statusf);
    let mut s = String::new();
    bufr.read_to_string(&mut s)?;
    let state: serde_json::Result<SavedState> = serde_json::from_str(s.as_str());

    let r = match state {
        Ok(s) => s,
        Err(orig_err) => {
            let state: serde_json::Result<crate::model_legacy::SavedState01> =
                serde_json::from_str(s.as_str());
            match state {
                Ok(s) => s.upconvert(),
                Err(_) => {
                    return Err(orig_err.into());
                }
            }
        }
    };

    Ok(Some(r))
}

/// lsblk: composefs:abc123..: not a block device
/// is what lsblk throws on composefs booted systems if we try to
/// get block devices using "/"
///
/// First, try to get the device from the `root` which is necessary
/// during installs as we don't want (or can't) to open up /sysroot or /boot
///
/// If that fails, it means we're not on the install path so we get the
/// device from checking mount point from /boot or /sysroot
#[context("Getting parent device")]
fn get_parent_device(root: &Dir) -> Result<Device> {
    match bootc_internal_blockdev::list_dev_by_dir(&root) {
        Ok(d) => Ok(d),
        Err(e) => {
            // Not really an error just yet
            log::debug!("{e:?}");
            list_dev_current_root()
        }
    }
}

impl SavedState {
    /// System-wide bootupd write lock (relative to sysroot).
    const WRITE_LOCK_PATH: &'static str = "run/bootupd-lock";
    /// Top-level directory for statefile (relative to sysroot).
    pub(crate) const STATEFILE_DIR: &'static str = "boot";
    /// On-disk bootloader statefile, akin to a tiny rpm/dpkg database,
    /// stored in `/boot` for Grub and in `ESP` for GrubCC
    pub(crate) const STATEFILE_NAME: &'static str = "bootupd-state.json";

    /// Try to acquire a system-wide lock to ensure non-conflicting state updates.
    ///
    /// While ordinarily the daemon runs as a systemd unit (which implicitly
    /// ensures a single instance) this is a double check against other
    /// execution paths.
    pub(crate) fn acquire_write_lock(
        sysroot_path: Utf8PathBuf,
        sysroot: Dir,
    ) -> Result<StateLockGuard> {
        sysroot
            .atomic_write_with_perms(Self::WRITE_LOCK_PATH, "", Permissions::from_mode(0o644))
            .context("Creating lock file")?;

        let lockfile = sysroot
            .open(Self::WRITE_LOCK_PATH)
            .context("Opening lock file")?
            .into_std();

        lockfile.lock_exclusive().context("Acquiring lock")?;

        let guard = StateLockGuard {
            sysroot_path,
            sysroot,
            termguard: Some(SignalTerminationGuard::new()?),
            lockfile: Some(lockfile),
        };
        Ok(guard)
    }

    /// Use this for cases when the target root isn't booted, which is
    /// offline installs.
    pub(crate) fn unlocked(sysroot_path: Utf8PathBuf, sysroot: Dir) -> Result<StateLockGuard> {
        Ok(StateLockGuard {
            sysroot_path,
            sysroot,
            termguard: None,
            lockfile: None,
        })
    }

    /// Load the JSON file containing on-disk state.
    #[context("Loading saved state")]
    pub(crate) fn load_from_disk(
        root_path: impl AsRef<Path>,
        bootloader: Option<Bootloader>,
    ) -> Result<Option<SavedState>> {
        let root_path = root_path.as_ref();

        let root = Dir::open_ambient_dir(root_path, ambient_authority())
            .with_context(|| format!("opening sysroot '{}'", root_path.display()))?;

        match bootloader {
            Some(b) => match b {
                Bootloader::Grub => {
                    let path = Path::new(Self::STATEFILE_DIR).join(Self::STATEFILE_NAME);

                    match root.open_optional(&path)? {
                        Some(f) => parse_statefile(f),
                        None => Ok(None),
                    }
                }

                Bootloader::GrubCC => {
                    let efi = Efi::default();

                    let device = get_parent_device(&root)?;

                    // Since we write the state file to all ESPs, it should be enough to get it
                    // from the first one. Though, we could check the integrity by getting from
                    // all the ESPs and making sure they're all the same...
                    let esp = device.find_first_colocated_esp()?;

                    // According to BLS, the ESP should be mounted at /boot or /boot/efi
                    // which the following method already checks
                    let mounted = efi
                        .ensure_mounted_esp(&Path::new("/"), &Path::new(&esp.path()))
                        .context("Mounting ESP")?;

                    let dir = Dir::open_ambient_dir(&mounted, ambient_authority())?;

                    match dir.open_optional(Self::STATEFILE_NAME)? {
                        Some(f) => parse_statefile(f),
                        None => Ok(None),
                    }
                }
            },

            // No bootloader, we're probably running inside a container
            None => Ok(None),
        }
    }

    /// Check whether statefile exists.
    pub(crate) fn ensure_not_present(
        root_path: impl AsRef<Path>,
        bootloader: Bootloader,
    ) -> Result<()> {
        let saved_state = SavedState::load_from_disk(&root_path, Some(bootloader))?;

        if saved_state.is_none() {
            return Ok(());
        }

        match bootloader {
            Bootloader::Grub => {
                let statepath = Path::new(root_path.as_ref())
                    .join(Self::STATEFILE_DIR)
                    .join(Self::STATEFILE_NAME);

                bail!("{} already exists", statepath.display());
            }

            Bootloader::GrubCC => {
                bail!("{} already exists in the ESP", Self::STATEFILE_NAME);
            }
        }
    }
}

/// Write-lock guard for statefile, protecting against concurrent state updates.
#[derive(Debug)]
pub(crate) struct StateLockGuard {
    pub(crate) sysroot_path: Utf8PathBuf,
    pub(crate) sysroot: Dir,
    #[allow(dead_code)]
    termguard: Option<SignalTerminationGuard>,
    #[allow(dead_code)]
    lockfile: Option<File>,
}

impl StateLockGuard {
    /// Atomically replace the on-disk state with a new version.
    #[context("Updating state")]
    pub(crate) fn update_state(
        &mut self,
        state: &SavedState,
        bootloader: Bootloader,
    ) -> Result<()> {
        if bootloader == Bootloader::Grub {
            let subdir = self.sysroot.open_dir(SavedState::STATEFILE_DIR)?;

            subdir
                .atomic_write_with_perms(
                    SavedState::STATEFILE_NAME,
                    serde_json::to_vec(state).context("Serializing state")?,
                    Permissions::from_mode(0o644),
                )
                .context("Writing state file")?;

            return Ok(());
        }

        let device = get_parent_device(&self.sysroot)?;
        let all_esps = device
            .find_colocated_esps()
            .context("Searching for ESP")?
            .ok_or_else(|| anyhow::anyhow!("ESP not found"))?;

        let efi = Efi::default();

        let serialized_state = serde_json::to_vec(state).context("Serializing state")?;

        for esp in all_esps {
            let mounted = efi
                .ensure_mounted_esp(&self.sysroot_path.as_std_path(), &Path::new(&esp.path()))
                .context("Mounting ESP")?;

            let dir = Dir::open_ambient_dir(&mounted, ambient_authority())?;

            dir.atomic_write_with_perms(
                SavedState::STATEFILE_NAME,
                &serialized_state,
                Permissions::from_mode(0o644),
            )
            .context("Writing state file")?;

            // Do the sync before unmount
            fsfreeze_thaw_cycle(dir.reopen_as_ownedfd()?)?;
            drop(dir);
            efi.unmount().context("unmount after update")?;
        }

        Ok(())
    }
}
