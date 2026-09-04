#!/bin/bash

# Test whether a VM created with BIOS only boots or not

set -eux

. ./helpers.sh

create_mount_device_bios

IMAGE=$1

podman run --rm --net=host --privileged --pid=host \
  --privileged \
  --security-opt label=type:unconfined_t \
  --env RUST_LOG=debug \
  --env BOOTC_BOOTLOADER_DEBUG=1 \
  -v /dev:/dev \
  -v /var/mnt:/var/mnt \
  "$IMAGE" \
    bootc install to-filesystem --karg console=ttyS0,115500n --skip-fetch-check \
    --acknowledge-destructive --disable-selinux /var/mnt
