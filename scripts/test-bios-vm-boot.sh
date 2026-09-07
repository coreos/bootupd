#!/bin/bash

# TODO(Johan-Liebert1): We can replace this entire thing with bcvk once we have
# https://github.com/bootc-dev/bootc/pull/2314
# which will let us use bootc install to-disk creating a BIOS partition

cd "$(dirname "$0")"

set -eux

IMAGE=$1

. ./helpers.sh

./test-bios-bootc-install.sh "$IMAGE"

if mount | grep -qF /var/mnt; then
    umount -R /var/mnt
fi

set +e

losetup -j "$DISK_IMAGE" | cut -d: -f1 | xargs -r losetup -d

set -e

SERIAL_LOG=$(mktemp /tmp/qemu-serial-XXXXXX.log)
trap 'cat "$SERIAL_LOG"' EXIT

echo "Booting '$DISK_IMAGE' with QEMU (BIOS mode, timeout=${BOOT_TIMEOUT}s)..."
echo "Serial log: $SERIAL_LOG"

qemu-system-x86_64 \
    -machine pc \
    -cpu host \
    -enable-kvm \
    -m 2048 \
    -display none \
    -serial file:"$SERIAL_LOG" \
    -drive file="$DISK_IMAGE",format=raw,if=virtio \
    -boot c \
    -no-reboot &

QEMU_PID=$!

boot_ok=0
elapsed=0

while [ "$elapsed" -lt "$BOOT_TIMEOUT" ]; do
    if ! kill -0 "$QEMU_PID" 2>/dev/null; then
        echo "QEMU exited prematurely after ${elapsed}s."
        break
    fi

    if grep -qiE 'login:|welcome to|reached target.*multi-user' "$SERIAL_LOG" 2>/dev/null; then
        boot_ok=1
        break
    fi

    sleep 5
    elapsed=$((elapsed + 5))
done

# Shut down QEMU
if kill -0 "$QEMU_PID" 2>/dev/null; then
    kill "$QEMU_PID"
    wait "$QEMU_PID" 2>/dev/null || true
fi

rm -rf "$DISK_IMAGE"

if [ "$boot_ok" -eq 1 ]; then
    echo "PASS: VM booted successfully (detected in ${elapsed}s)."
    exit 0
else
    echo "FAIL: VM did not boot within ${BOOT_TIMEOUT}s."
    exit 1
fi
