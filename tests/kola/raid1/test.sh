#!/bin/bash
## kola:
##   # additionalDisks is only supported on qemu.
##   platforms: qemu
##   # Root reprovisioning requires at least 4GiB of memory.
##   minMemory: 4096
##   # Linear RAID is setup on these disks.
##   additionalDisks: ["10G"]
##   # This test includes a lot of disk I/O and needs a higher
##   # timeout value than the default.
##   timeoutMin: 15
##   description: Verify updating multiple EFIs with RAID 1 works.

set -xeuo pipefail

# shellcheck disable=SC1091
. "$KOLA_EXT_DATA/libtest.sh"

srcdev=$(findmnt -nvr /sysroot -o SOURCE)
[[ ${srcdev} == "/dev/md126" ]]

blktype=$(lsblk -o TYPE "${srcdev}" --noheadings)
[[ ${blktype} == raid1 ]]

fstype=$(findmnt -nvr /sysroot -o FSTYPE)
[[ ${fstype} == xfs ]]
ok "source is XFS on RAID1 device"


mount -o remount,rw /boot

rm -f -v /boot/bootupd-state.json

bootupctl adopt-and-update | grep "Adopted and updated: EFI"

bootupctl status | grep "Component EFI"
ok "bootupctl adopt-and-update supports multiple EFIs on RAID1"
