use rustix::fd::AsFd;
use rustix::ffi as c;
use rustix::io::Errno;
use rustix::ioctl::opcode;
use rustix::{io, ioctl};

use crate::util::SignalTerminationGuard;

fn ioctl_fifreeze<Fd: AsFd>(fd: Fd) -> io::Result<()> {
    // SAFETY: `FIFREEZE` is a no-argument opcode.
    // `FIFREEZE` is defined as `_IOWR('X', 119, int)`.
    unsafe {
        let ctl = ioctl::NoArg::<{ opcode::read_write::<c::c_int>(b'X', 119) }>::new();
        ioctl::ioctl(fd, ctl)
    }
}

fn ioctl_fithaw<Fd: AsFd>(fd: Fd) -> io::Result<()> {
    // SAFETY: `FITHAW` is a no-argument opcode.
    // `FITHAW` is defined as `_IOWR('X', 120, int)`.
    unsafe {
        let ctl = ioctl::NoArg::<{ opcode::read_write::<c::c_int>(b'X', 120) }>::new();
        ioctl::ioctl(fd, ctl)
    }
}

/// syncfs() doesn't flush the journal on XFS,
/// and since GRUB2 can't read the XFS journal, the system
/// could fail to boot.
///
/// http://marc.info/?l=linux-fsdevel&m=149520244919284&w=2
/// https://github.com/ostreedev/ostree/pull/1049
///
/// This function always call syncfs() first, then calls
/// `ioctl(FIFREEZE)` and `ioctl(FITHAW)`, ignoring `EOPNOTSUPP` and `EPERM`
pub(crate) fn fsfreeze_thaw_cycle<Fd: AsFd>(fd: Fd) -> anyhow::Result<()> {
    rustix::fs::syncfs(&fd)?;

    let _guard = SignalTerminationGuard::new()?;

    let freeze = ioctl_fifreeze(&fd);
    match freeze {
        // Ignore permissions errors (tests)
        Err(Errno::PERM) => Ok(()),
        // Ignore unsupported FS
        Err(Errno::NOTSUP) => Ok(()),
        Ok(()) => Ok(ioctl_fithaw(fd)?),
        _ => Ok(freeze?),
    }
}
