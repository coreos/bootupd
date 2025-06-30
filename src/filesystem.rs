use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::process::Command;

use anyhow::Result;
use fn_error_context::context;
use rustix::fd::BorrowedFd;
use serde::Deserialize;

use crate::bootc_utils::CommandRunExt;

#[derive(Deserialize, Debug)]
#[serde(rename_all = "kebab-case")]
#[allow(dead_code)]
pub(crate) struct Filesystem {
    pub(crate) source: String,
    pub(crate) fstype: String,
    pub(crate) options: String,
    pub(crate) uuid: Option<String>,
}

#[derive(Deserialize, Debug)]
pub(crate) struct Findmnt {
    pub(crate) filesystems: Vec<Filesystem>,
}

#[context("Inspecting filesystem {path:?}")]
pub(crate) fn inspect_filesystem(root: &openat::Dir, path: &str) -> Result<Filesystem> {
    let rootfd = unsafe { BorrowedFd::borrow_raw(root.as_raw_fd()) };
    // SAFETY: This is unsafe just for the pre_exec, when we port to cap-std we can use cap-std-ext
    let o: Findmnt = unsafe {
        Command::new("findmnt")
            .args(["-J", "-v", "--output=SOURCE,FSTYPE,OPTIONS,UUID", path])
            .pre_exec(move || rustix::process::fchdir(rootfd).map_err(Into::into))
            .run_and_parse_json()?
    };
    o.filesystems
        .into_iter()
        .next()
        .ok_or_else(|| anyhow::anyhow!("findmnt returned no data"))
}
