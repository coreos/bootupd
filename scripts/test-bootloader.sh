#!/bin/bash

set -eux

IMG_NAME=$1
BOOTLOADER=$2

case $BOOTLOADER in
    systemd)
        EFI_DIR_NAME=systemd-boot
        ;;
    grub-cc)
        EFI_DIR_NAME=grub-cc
        ;;
    grub)
        EFI_DIR_NAME=grub2
        ;;
    *)
        echo "Unknown bootloader $BOOTLOADER"
        exit 1
        ;;
esac

cat <<-EOF > sfdisk-buf
label: gpt
label-id:  65be9332-59ba-11f1-9b26-6a8e2ab625e4
size=1Gib, type=C12A7328-F81F-11D2-BA4B-00A0C93EC93B, name="EFI-SYSTEM"
           type=4F68BCE3-E8CD-4DB1-96E7-FBCAF984B709, name="root"
EOF

truncate -s4G "${BOOTLOADER}-test.img"

cat sfdisk-buf | sfdisk --wipe=always "${BOOTLOADER}-test.img"

mkdir -p /var/mnt

# Also update kernel partition tables
loopdev=$(losetup --find --show --partscan "${BOOTLOADER}-test.img")
sleep 1

mkfs.vfat "${loopdev}p1"
mkfs.ext4 "${loopdev}p2"

mount "${loopdev}p2" /var/mnt

ESP="/var/mnt/efi"

mkdir -p $ESP
mount "${loopdev}p1" $ESP


# Test installing the bootloader
podman run --rm --net=host --pid=host \
  --privileged \
  --security-opt label=type:unconfined_t \
  --env RUST_LOG=trace \
  -v /dev:/dev \
  -v /var/mnt:/var/mnt \
  "$IMG_NAME" \
  bootupctl backend install --bootloader "$BOOTLOADER" /var/mnt -vvvv

# Make sure bootupd-state.json is in the esp
test -f "$ESP/bootupd-state.json"

cat "$ESP/bootupd-state.json" | jq

version=$(cat "$ESP/bootupd-state.json" | jq -r ".installed.EFI.meta.version")

if [[ $version != *shim* ]]; then echo "shim not found in version"; exit 1; fi
if [[ $version != *"$EFI_DIR_NAME"* ]]; then echo "$BOOTLOADER not found in version"; exit 1; fi

# Test if the correct binary has been installed
actualShasum=$(podman run --rm "$IMG_NAME" find "/usr/lib/efi/$EFI_DIR_NAME" -type f -exec sha512sum {} + | awk '{print $1}')
actualShasum="sha512:$actualShasum"

if [[ $(uname -m) == "x86_64" ]]; then
    grubName="grubx64.efi"
else
    grubName="grubaa64.efi"
fi

# TODO: Remove hardcoded "fedora" once we have support in centos
storedShasum=$(cat "$ESP/bootupd-state.json" | jq -r --arg grub "$grubName" '.installed.EFI.filetree.children["fedora/\($grub)"].sha512')

test "$actualShasum" == "$storedShasum"

efiBinShasum=$(find "$ESP" -type f -name "$grubName" -exec sha512sum {} + | awk '{print $1}')
efiBinShasum="sha512:$efiBinShasum"

test "$efiBinShasum" == "$actualShasum"

umount -Rl /var/mnt
