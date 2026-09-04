#!/bin/bash

set -u
IMG_NAME=$1

set -x

. ./helpers.sh

run_podman() {
    set +eu

    local bootloader=("$@")

    podman run --rm --net=host --privileged --pid=host \
      --privileged \
      --security-opt label=type:unconfined_t \
      --env RUST_LOG=trace \
      -v /dev:/dev \
      -v /var/mnt:/var/mnt \
      "$IMG_NAME" \
      bootupctl backend install "${bootloader[@]}" --device /dev/loop0 --auto /var/mnt -vvvv
}

create_mount_device_bios
# This SHOULD fail
if run_podman "--bootloader" "systemd"; then echo "Bootloader systemd with BIOS should have failed"; exit 1; fi

create_mount_device_bios
# This SHOULD also fail
if run_podman "--bootloader" "grub-cc"; then echo "Bootloader GrubCC with BIOS should have failed"; exit 1; fi

create_mount_device_bios
# This SHOULD pass
if ! run_podman "--bootloader" "grub"; then echo "Bootloader Grub with BIOS should have passed"; exit 1; fi

create_mount_device_bios
# This SHOULD pass
if ! run_podman; then echo "Bootloader None (auto detected as grub) with BIOS should have passed"; exit 1; fi
