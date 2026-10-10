#!/bin/bash
## kola:
##   # additionalDisks is only supported on qemu.
##   platforms: qemu
##   # RAID 1 is setup on these disks.
##   additionalDisks: ["10G"]
##   architectures: "aarch64"
##   minMemory: 4096
##   description: Verify that using the bootupd varlink interface to sync capsule updates works.
##   creationDate: 2026-08-21

# shellcheck disable=SC1091
. "${KOLA_EXT_DATA}/libtest_varlink.sh"

check_basic
check_raid

ok "checks with multiple ESPs successful"
