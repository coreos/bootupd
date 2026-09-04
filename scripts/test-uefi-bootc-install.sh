#!/bin/bash

cd "$(dirname "$0")"

set -eux

IMAGE=$1
BACKEND=$2

composefs=()

if [[ "$BACKEND" == "composefs" ]]; then
    composefs=(--composefs-backend)
fi

truncate -s10G /var/disk.img

# We don't have bootupd support for GrubCC and SystemdBoot
# in bootc yet
podman run --rm --net=host --pid=host \
  --privileged \
  --security-opt label=type:unconfined_t \
  --env RUST_LOG=debug \
  --env BOOTC_BOOTLOADER_DEBUG=1 \
  -v /dev:/dev \
  "$IMAGE" \
    bootc install to-filesystem "${composefs[@]}" --karg console=ttyS0,115200n8 --skip-fetch-check \
    --generic-image --disable-selinux /var/test-img.img
