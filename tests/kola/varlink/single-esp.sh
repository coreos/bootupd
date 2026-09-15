#!/bin/bash
## kola:
##   exclusive: false
##   description: Verify the bootupd varlink service is enabled,
##                and fails when used incorrectly.
##   tags: "platform-independent"
##   creationDate: 2026-08-21

# shellcheck disable=SC1091
. "${KOLA_EXT_DATA}/libtest_varlink.sh"

if [ ! -d /sys/firmware/efi ]; then
    echo "Not an EFI system - skipping"
    exit 0
fi

check_basic
check_no_raid

ok "checks with a single ESP successful"
