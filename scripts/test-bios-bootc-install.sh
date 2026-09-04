#!/bin/bash

# Test whether a VM created with BIOS only boots or not

cd "$(dirname "$0")"

set -eux

. ./helpers.sh

create_mount_device_bios

IMAGE=$1

podman run --rm --net=host --pid=host \
  --privileged \
  --security-opt label=type:unconfined_t \
  --env RUST_LOG=debug \
  --env BOOTC_BOOTLOADER_DEBUG=1 \
  -v /dev:/dev \
  -v /var/mnt:/var/mnt \
  "$IMAGE" \
    bootc install to-filesystem --bootloader=none --karg console=ttyS0,115500n --skip-fetch-check \
    --acknowledge-destructive --disable-selinux /var/mnt

# Make sure the mount is actually writable
mount -o remount,rw /var/mnt/boot

run_bootupctl_bios "$IMAGE"
