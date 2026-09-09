#!/bin/bash

cd "$(dirname "$0")"

set -eux

. ./helpers.sh

IMAGE=$1
BACKEND=$2

composefs=()

if [[ "$BACKEND" == "composefs" ]]; then
    composefs=(--composefs-backend)
fi

truncate -s10G "$DISK_IMAGE"

# We don't have bootupd support for GrubCC and SystemdBoot
# in bootc yet
podman run --rm --net=host --pid=host \
  --privileged \
  --security-opt label=type:unconfined_t \
  --env RUST_LOG=debug \
  --env BOOTC_BOOTLOADER_DEBUG=1 \
  -v /dev:/dev \
  -v "$DISK_IMAGE":"$DISK_IMAGE" \
  "$IMAGE" \
    bootc install to-disk --filesystem=ext4 --wipe \
    "${composefs[@]}" --karg console=ttyS0,115200n8 \
    --generic-image --via-loopback --disable-selinux "$DISK_IMAGE"
