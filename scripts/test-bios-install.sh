#!/bin/bash

cd "$(dirname "$0")"

set -u
IMG_NAME=$1

set -x

. ./helpers.sh

create_mount_device_bios
# This SHOULD fail
if run_bootupctl_bios "$IMG_NAME"  "--bootloader" "systemd"; then echo "Bootloader systemd with BIOS should have failed"; exit 1; fi

create_mount_device_bios
# This SHOULD also fail
if run_bootupctl_bios  "$IMG_NAME" "--bootloader" "grub-cc"; then echo "Bootloader GrubCC with BIOS should have failed"; exit 1; fi

create_mount_device_bios
# This SHOULD pass
if ! run_bootupctl_bios "$IMG_NAME" "--bootloader" "grub"; then echo "Bootloader Grub with BIOS should have passed"; exit 1; fi

create_mount_device_bios
# This SHOULD pass
if ! run_bootupctl_bios "$IMG_NAME"; then echo "Bootloader None (auto detected as grub) with BIOS should have passed"; exit 1; fi
