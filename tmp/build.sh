#! /usr/bin/env bash

set -euo pipefail

rm -f bootupctl
cp ../target/debug/bootupd ./bootupctl
podman build -f Containerfile -t fedora-sdboot:latest .
rm -f fedora-sdboot.tar
podman save -o fedora-sdboot.tar fedora-sdboot:latest
IMAGE_ID=$(sudo podman load -i fedora-sdboot.tar | awk '/Loaded image:/ {print $3}')

if [ ! -e ./bootable.img ] ; then
    fallocate -l 20G bootable.img
fi

sudo podman run \
    --rm --privileged --pid=host \
    -it \
    -v /sys/fs/selinux:/sys/fs/selinux \
    -v /etc/containers:/etc/containers:Z \
    -v /var/lib/containers:/var/lib/containers \
    -v /dev:/dev \
    -v .:/data:Z \
    --security-opt label=type:unconfined_t \
    $IMAGE_ID bootc install to-disk --via-loopback /data/bootable.img --filesystem ext4 --wipe
