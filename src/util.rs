use std::collections::HashSet;
use std::os::fd::{AsRawFd, BorrowedFd};
use std::path::Path;
use std::process::Command;

use anyhow::{bail, Context, Result};
use cap_std_ext::cap_std::fs::Dir;
use openat_ext::OpenatDirExt;

pub(crate) trait CommandRunExt {
    fn run(&mut self) -> Result<()>;
}

impl CommandRunExt for Command {
    fn run(&mut self) -> Result<()> {
        let r = self.status()?;
        if !r.success() {
            bail!("Child [{:?}] exited: {}", self, r);
        }
        Ok(())
    }
}

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

pub(crate) fn filenames(dir: &openat::Dir) -> Result<HashSet<String>> {
    let mut ret = HashSet::new();
    for entry in dir.list_dir(".")? {
        let entry = entry?;
        let name = if let Some(name) = entry.file_name().to_str() {
            name
        } else {
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

pub(crate) fn ensure_writable_mount<P: AsRef<Path>>(p: P) -> Result<()> {
    let p = p.as_ref();
    let stat = rustix::fs::statvfs(p)?;
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

// Re-open an [`openat::Dir`] via the cap-std version.
pub(crate) fn reopen_dir(d: &openat::Dir) -> Result<Dir> {
    Dir::reopen_dir(&unsafe { BorrowedFd::borrow_raw(d.as_raw_fd()) }).map_err(Into::into)
}

// Re-open an [`cap_std::fs::Dir`] as a legacy openat::Dir.
pub(crate) fn reopen_legacy_dir(d: &Dir) -> Result<openat::Dir> {
    openat::Dir::open(format!("/proc/self/fd/{}", d.as_raw_fd())).map_err(Into::into)
}
