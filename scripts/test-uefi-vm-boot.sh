#!/bin/bash

set -eux

IMAGE=$1
BACKEND=$2
DISK_IMAGE=/var/test-img.img
TIMEOUT=300

./test-uefi-bootc-install.sh "$IMAGE" "$BACKEND"

set +e

umount -R /var/mnt 2>/dev/null || true
losetup -j "$DISK_IMAGE" | cut -d: -f1 | xargs -r losetup -d

set -e

# Find OVMF firmware
OVMF_CODE=/usr/share/OVMF/OVMF_CODE_4M.fd
if [ ! -f "$OVMF_CODE" ]; then
    OVMF_CODE=/usr/share/OVMF/OVMF_CODE.fd
fi

OVMF_VARS_TEMPLATE=/usr/share/OVMF/OVMF_VARS_4M.fd
if [ ! -f "$OVMF_VARS_TEMPLATE" ]; then
    OVMF_VARS_TEMPLATE=/usr/share/OVMF/OVMF_VARS.fd
fi

OVMF_VARS=$(mktemp /tmp/ovmf-vars-XXXXXX.fd)
cp "$OVMF_VARS_TEMPLATE" "$OVMF_VARS"

SERIAL_LOG=$(mktemp /tmp/qemu-serial-XXXXXX.log)
trap 'cat "$SERIAL_LOG"; rm -f "$OVMF_VARS"' EXIT

echo "Booting '$DISK_IMAGE' with QEMU (UEFI mode, timeout=${TIMEOUT}s)..."
echo "Serial log: $SERIAL_LOG"

qemu-system-x86_64 \
    -machine q35 \
    -cpu host \
    -enable-kvm \
    -m 2048 \
    -nographic \
    -serial file:"$SERIAL_LOG" \
    -drive if=pflash,format=raw,readonly=on,file="$OVMF_CODE" \
    -drive if=pflash,format=raw,file="$OVMF_VARS" \
    -drive file="$DISK_IMAGE",format=raw,if=virtio \
    -boot c \
    -no-reboot &

QEMU_PID=$!

boot_ok=0
elapsed=0

while [ "$elapsed" -lt "$TIMEOUT" ]; do
    if ! kill -0 "$QEMU_PID" 2>/dev/null; then
        echo "QEMU exited prematurely after ${elapsed}s."
        break
    fi

    if grep -qiE 'login:|reached target.*multi-user' "$SERIAL_LOG" 2>/dev/null; then
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

rm -f "$DISK_IMAGE"

if [ "$boot_ok" -eq 1 ]; then
    echo "PASS: VM booted successfully in UEFI mode (detected in ${elapsed}s)."
    exit 0
else
    echo "FAIL: VM did not boot within ${TIMEOUT}s."
    exit 1
fi
