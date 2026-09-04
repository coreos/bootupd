#!/bin/bash

# TODO(Johan-Liebert1): We can replace this entire thing with bcvk once we have
# https://github.com/bootc-dev/bootc/pull/2314
# which will let us use bootc install to-disk creating a BIOS partition

set -ux
set +e

IMAGE=$1
DISK_IMAGE=/var/test-img.img
TIMEOUT=300

./test-bios-bootc-install.sh "$IMAGE"

umount -R /var/mnt 2>/dev/null || true
losetup -j "$DISK_IMAGE" | cut -d: -f1 | xargs -r losetup -d

set -e

SERIAL_LOG=$(mktemp /tmp/qemu-serial-XXXXXX.log)
trap 'cat "$SERIAL_LOG"' EXIT

echo "Booting '$DISK_IMAGE' with QEMU (BIOS mode, timeout=${TIMEOUT}s)..."
echo "Serial log: $SERIAL_LOG"

qemu-system-x86_64 \
    -machine pc \
    -cpu host \
    -enable-kvm \
    -m 2048 \
    -nographic \
    -serial file:"$SERIAL_LOG" \
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

cat "$SERIAL_LOG"

if [ "$boot_ok" -eq 1 ]; then
    echo "PASS: VM booted successfully (detected in ${elapsed}s)."
    exit 0
else
    echo "FAIL: VM did not boot within ${TIMEOUT}s."
    exit 1
fi
