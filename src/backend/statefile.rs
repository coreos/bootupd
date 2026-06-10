//! On-disk saved state.

use crate::bootloader::Bootloader;
use crate::efi::Efi;
use crate::freezethaw::fsfreeze_thaw_cycle;
use crate::model::SavedState;
use crate::util::SignalTerminationGuard;
use anyhow::{bail, Context, Result};
use cap_std::ambient_authority;
use cap_std::fs::{Dir, Permissions, PermissionsExt};
use cap_std_ext::dirext::CapStdExtDirExt;
use fn_error_context::context;
use fs2::FileExt;
use std::fs::File;
use std::io::prelude::*;
use std::os::fd::{AsRawFd, FromRawFd};
use std::path::Path;
use tempfile::tempdir;

impl SavedState {
    /// System-wide bootupd write lock (relative to sysroot).
    const WRITE_LOCK_PATH: &'static str = "run/bootupd-lock";
    /// Top-level directory for statefile (relative to sysroot).
    pub(crate) const STATEFILE_DIR: &'static str = "boot";
    /// On-disk bootloader statefile, akin to a tiny rpm/dpkg database, stored in `/boot`.
    pub(crate) const STATEFILE_NAME: &'static str = "bootupd-state.json";

    /// Try to acquire a system-wide lock to ensure non-conflicting state updates.
    ///
    /// While ordinarily the daemon runs as a systemd unit (which implicitly
    /// ensures a single instance) this is a double check against other
    /// execution paths.
    pub(crate) fn acquire_write_lock(sysroot: Dir) -> Result<StateLockGuard> {
        sysroot
            .atomic_write_with_perms(Self::WRITE_LOCK_PATH, "", Permissions::from_mode(0o644))
            .context("Creating lock file")?;

        let lockfile = sysroot
            .open(Self::WRITE_LOCK_PATH)
            .context("Opening lock file")?
            .into_std();

        lockfile.lock_exclusive().context("Acquiring lock")?;

        let guard = StateLockGuard {
            sysroot,
            termguard: Some(SignalTerminationGuard::new()?),
            lockfile: Some(lockfile),
        };
        Ok(guard)
    }

    /// Use this for cases when the target root isn't booted, which is
    /// offline installs.
    pub(crate) fn unlocked(sysroot: Dir) -> Result<StateLockGuard> {
        Ok(StateLockGuard {
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
        let sysroot = Dir::open_ambient_dir(root_path, ambient_authority())
            .with_context(|| format!("opening sysroot '{}'", root_path.display()))?;

        let (statefile, _esp_guard) = match bootloader {
            Some(b) => match b {
                Bootloader::Grub => {
                    let path = Path::new(Self::STATEFILE_DIR).join(Self::STATEFILE_NAME);
                    (sysroot.open_optional(&path)?, None)
                }

                Bootloader::GrubCC => {
                    let efi = Efi::default();

                    let dir = Dir::open_ambient_dir(&root_path, ambient_authority())
                        .with_context(|| format!("Opening filesystem path {root_path:?}"))?;
                    let device = bootc_internal_blockdev::list_dev_by_dir(&dir)?;

                    // Since we write the state file to all ESPs, it should be enough to get it
                    // from the first one. Though, we could check the integrity by getting from
                    // all the ESPs and making sure they're all the same...
                    let esp = device.find_first_colocated_esp()?;

                    let tmpdir = tempdir()?;
                    std::fs::create_dir_all(tmpdir.path().join("efi"))
                        .context("Creating efi inside tmpdir")?;

                    let mounted = efi
                        .ensure_mounted_esp(tmpdir.path(), &Path::new(&esp.path()))
                        .context("Mounting ESP")?;

                    let dir = Dir::open_ambient_dir(&mounted, ambient_authority())?;

                    (dir.open_optional(Self::STATEFILE_NAME)?, Some(efi))
                }
            },

            // No bootloader, we're probably running inside a container
            None => (None, None),
        };

        let saved_state = if let Some(statusf) = statefile {
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
            Some(r)
        } else {
            None
        };

        Ok(saved_state)
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

        let dir = unsafe { Dir::from_raw_fd(self.sysroot.as_raw_fd()) };
        let device = bootc_internal_blockdev::list_dev_by_dir(&dir)?;
        let all_esps = device
            .find_colocated_esps()
            .context("Searching for ESP")?
            .ok_or_else(|| anyhow::anyhow!("ESP not found"))?;

        let efi = Efi::default();

        let tmpdir = tempdir()?;

        // [`ensure_mounted_esp`] needs this
        std::fs::create_dir_all(tmpdir.path().join("efi")).context("Creating efi inside tmpdir")?;

        for esp in all_esps {
            let mounted = efi
                .ensure_mounted_esp(tmpdir.path(), &Path::new(&esp.path()))
                .context("Mounting ESP")?;

            let dir = Dir::open_ambient_dir(&mounted, ambient_authority())?;

            dir.atomic_replace_with(SavedState::STATEFILE_NAME, |w| -> std::io::Result<()> {
                serde_json::to_writer(w, state)?;
                Ok(())
            })?;

            // dir.set_permissions(SavedState::STATEFILE_NAME, 0o644)?;

            // Do the sync before unmount
            fsfreeze_thaw_cycle(dir.reopen_as_ownedfd()?)?;
            drop(dir);
            efi.unmount().context("unmount after update")?;
        }

        Ok(())
    }
}
