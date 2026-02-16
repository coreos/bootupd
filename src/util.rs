use std::collections::HashSet;
use std::os::unix::io::AsRawFd;
use std::path::Path;
use std::process::Command;

use anyhow::{bail, Context, Result};
use openat_ext::OpenatDirExt;
use rustix::fd::BorrowedFd;

/// Parse an environment variable as UTF-8
#[allow(dead_code)]
pub(crate) fn getenv_utf8(n: &str) -> Result<Option<String>> {
    if let Some(v) = std::env::var_os(n) {
        Ok(Some(
            v.to_str()
                .ok_or_else(|| anyhow::anyhow!("{} is invalid UTF-8", n))?
                .to_string(),
        ))
    } else {
        Ok(None)
    }
}

#[allow(dead_code)]
pub(crate) fn filenames(dir: &openat::Dir) -> Result<HashSet<String>> {
    let mut ret = HashSet::new();
    for entry in dir.list_dir(".")? {
        let entry = entry?;
        let Some(name) = entry.file_name().to_str() else {
            bail!("Invalid UTF-8 filename: {:?}", entry.file_name())
        };
        match dir.get_file_type(&entry)? {
            openat::SimpleType::File => {
                ret.insert(format!("/{name}"));
            }
            openat::SimpleType::Dir => {
                let child = dir.sub_dir(name)?;
                for mut k in filenames(&child)?.drain() {
                    k.reserve(name.len() + 1);
                    k.insert_str(0, name);
                    k.insert(0, '/');
                    ret.insert(k);
                }
            }
            openat::SimpleType::Symlink => {
                bail!("Unsupported symbolic link {:?}", entry.file_name())
            }
            openat::SimpleType::Other => {
                bail!("Unsupported non-file/directory {:?}", entry.file_name())
            }
        }
    }
    Ok(ret)
}

/// Return the available space in bytes on the filesystem containing the given directory.
/// Uses f_bavail * f_frsize from fstatvfs to avoid partial updates when the partition is full.
pub(crate) fn available_space_bytes(dir: &openat::Dir) -> Result<u64> {
    let fd = unsafe { BorrowedFd::borrow_raw(dir.as_raw_fd()) };
    let st = rustix::fs::fstatvfs(fd)?;
    Ok((st.f_bavail as u64) * (st.f_frsize as u64))
}

pub(crate) fn ensure_writable_mount<P: AsRef<Path>>(p: P) -> Result<()> {
    let p = p.as_ref();
    let stat =
        rustix::fs::statvfs(p).map_err(|e| std::io::Error::from_raw_os_error(e.raw_os_error()))?;
    if !stat.f_flag.contains(rustix::fs::StatVfsMountFlags::RDONLY) {
        return Ok(());
    }
    let status = std::process::Command::new("mount")
        .args(["-o", "remount,rw"])
        .arg(p)
        .status()?;
    if !status.success() {
        anyhow::bail!("Failed to remount {:?} writable", p);
    }
    Ok(())
}

/// Runs the provided Command object, captures its stdout, and swallows its stderr except on
/// failure. Returns a Result<String> describing whether the command failed, and if not, its
/// standard output. Output is assumed to be UTF-8. Errors are adequately prefixed with the full
/// command.
#[allow(dead_code)]
pub(crate) fn cmd_output(cmd: &mut Command) -> Result<String> {
    let result = cmd
        .output()
        .with_context(|| format!("running {:#?}", cmd))?;
    if !result.status.success() {
        eprintln!("{}", String::from_utf8_lossy(&result.stderr));
        bail!("{:#?} failed with {}", cmd, result.status);
    }
    String::from_utf8(result.stdout)
        .with_context(|| format!("decoding as UTF-8 output of `{:#?}`", cmd))
}

/// Copy from https://github.com/containers/bootc/blob/main/ostree-ext/src/container_utils.rs#L20
/// Attempts to detect if the current process is running inside a container.
/// This looks for the `container` environment variable or the presence
/// of Docker or podman's more generic `/run/.containerenv`.
/// This is a best-effort function, as there is not a 100% reliable way
/// to determine this.
pub fn running_in_container() -> bool {
    if std::env::var_os("container").is_some() {
        return true;
    }
    // https://stackoverflow.com/questions/20010199/how-to-determine-if-a-process-runs-inside-lxc-docker
    for p in ["/run/.containerenv", "/.dockerenv"] {
        if Path::new(p).exists() {
            return true;
        }
    }
    false
}

/// Suppress SIGTERM while active
// TODO: In theory we could record if we got SIGTERM and exit
// on drop, but in practice we don't care since we're going to exit anyways.
#[derive(Debug)]
pub(crate) struct SignalTerminationGuard(signal_hook_registry::SigId);

impl SignalTerminationGuard {
    pub(crate) fn new() -> Result<Self> {
        let signal = unsafe { signal_hook_registry::register(libc::SIGTERM, || {})? };
        Ok(Self(signal))
    }
}

impl Drop for SignalTerminationGuard {
    fn drop(&mut self) {
        signal_hook_registry::unregister(self.0);
    }
}
