#!/bin/bash
# Smoke test for bcvk ephemeral (virtiofs direct-boot) environments.
# This runs *inside* the ephemeral VM and verifies that bootupd
# handles the diskless virtiofs root gracefully.
set -xeuo pipefail

# Verify we're actually on virtiofs — this test is meaningless otherwise.
root_fstype=$(findmnt -n -o FSTYPE /)
if [ "$root_fstype" != "virtiofs" ]; then
    echo "ERROR: expected root fstype 'virtiofs', got '${root_fstype}'" >&2
    exit 1
fi
echo "ok: root filesystem is virtiofs"

# The bootloader-update.service should have already run at boot (it's
# enabled by preset on Fedora). Verify it succeeded rather than failed.
systemctl is-active bootloader-update.service
echo "ok: bootloader-update.service is active (ran successfully at boot)"

# Also verify a manual invocation skips cleanly.
output=$(bootupctl update 2>&1)
echo "$output"
if ! echo "$output" | grep -qi 'skipping'; then
    echo "ERROR: expected skip message in output" >&2
    exit 1
fi
echo "ok: bootupctl update skipped cleanly on virtiofs"

echo "All ephemeral smoke tests passed."
