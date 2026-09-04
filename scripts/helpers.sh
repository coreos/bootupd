#!/bin/bash

create_mount_device_bios() {
    set +e

    umount -R /var/mnt
    losetup -j /var/test-img.img | cut -d: -f1 | xargs -r losetup -d

    set -e

    rm -rfv /var/test-img.img

    cat <<-EOF > sfdisk-buf
label: gpt
label-id:  65be9332-59ba-11f1-9b26-6a8e2ab625e4
size=1Mib, type=21686148-6449-6E6F-744E-656564454649, name="BIOS"
size=1Gib, type=0FC63DAF-8483-4772-8E79-3D69D8477DE4, name="boot"
           type=4F68BCE3-E8CD-4DB1-96E7-FBCAF984B709, name="root"
EOF

    truncate -s10G /var/test-img.img

    cat sfdisk-buf | sfdisk --wipe=always /var/test-img.img

    mkdir -p /var/mnt

    # Also update kernel partition tables
    loopdev=$(losetup --find --show --partscan /var/test-img.img)
    sleep 1

    mkfs.ext4 "${loopdev}p3"
    mount "${loopdev}p3" /var/mnt

    # Need this for bootupd state
    mkdir -p /var/mnt/boot

    mkfs.ext4 "${loopdev}p2"
    mount "${loopdev}p2" /var/mnt/boot
}

run_bootupctl_bios() {
    set +eu

    IMG_NAME=$1

    DEVICE=$(losetup -j /var/test-img.img | cut -d: -f1)

    # Skip IMG_NAME
    local bootloader=("${@:2}")

    podman run --rm --net=host --privileged --pid=host \
      --privileged \
      --security-opt label=type:unconfined_t \
      --env RUST_LOG=trace \
      -v /dev:/dev \
      -v /var/mnt:/var/mnt \
      "$IMG_NAME" \
      bootupctl backend install "${bootloader[@]}" --write-uuid --device "$DEVICE" --component BIOS /var/mnt -vvvv
}
