//! On-disk saved state.

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
use std::path::Path;

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
    pub(crate) fn load_from_disk(root_path: impl AsRef<Path>) -> Result<Option<SavedState>> {
        let root_path = root_path.as_ref();
        let sysroot = Dir::open_ambient_dir(root_path, ambient_authority())
            .with_context(|| format!("opening sysroot '{}'", root_path.display()))?;

        let statefile_path = Path::new(Self::STATEFILE_DIR).join(Self::STATEFILE_NAME);
        let saved_state = if let Some(statusf) = sysroot.open_optional(&statefile_path)? {
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
    pub(crate) fn ensure_not_present(root_path: impl AsRef<Path>) -> Result<()> {
        let statepath = Path::new(root_path.as_ref())
            .join(Self::STATEFILE_DIR)
            .join(Self::STATEFILE_NAME);
        if statepath.exists() {
            bail!("{} already exists", statepath.display());
        }
        Ok(())
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
    pub(crate) fn update_state(&mut self, state: &SavedState) -> Result<()> {
        let subdir = self.sysroot.open_dir(SavedState::STATEFILE_DIR)?;

        subdir
            .atomic_write_with_perms(
                SavedState::STATEFILE_NAME,
                serde_json::to_vec(state).context("Serializing state")?,
                Permissions::from_mode(0o644),
            )
            .context("Writing state file")?;

        Ok(())
    }
}
