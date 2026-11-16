#!/bin/bash
set -xeuo pipefail

# Verify that bootupctl status running in bootc container
if [ ! -d "/sysroot/ostree/repo/" ]; then
  echo "Error: should run test in bootc container"
  exit 100
fi

# check if running in container
if [ "$container" ] || [ -f /run/.containerenv ] || [ -f /.dockerenv ]; then
  arch="$(uname --machine)"
  if [[ "${arch}" == "x86_64" ]]; then
    components_text='Available components: BIOS EFI'
    components_json='{"components":["BIOS","EFI"]}'
  else
    # Assume aarch64 for now
    components_text='Available components: EFI'
    components_json='{"components":["EFI"]}'
  fi

  output=$(bootupctl status | tr -d '\r')
  [ "${components_text}" == "${output}" ]
  output=$(bootupctl status --json)
  [ "${components_json}" == "${output}" ]

  # The CentOS Stream images carry GRUB. Dockerfile.bls adds a systemd-boot
  # component, and a grub-cc one while grub-cc testing is enabled, and makes
  # GRUB the default.
  if [ -d /usr/lib/efi/systemd-boot ]; then
    bootloaders='"grub"'
    if [ -d /usr/lib/efi/grub-cc ]; then
      bootloaders+=',"grub-cc"'
    fi
    bootloaders_json="{\"bootloaders\":[${bootloaders},\"systemd\"],\"default-bootloader\":\"grub\"}"
  else
    bootloaders_json='{"bootloaders":["grub"],"default-bootloader":null}'
  fi
  output=$(bootupctl backend list-bootloaders --json)
  [ "${bootloaders_json}" == "${output}" ]
  # The same for an install of the EFI component alone
  output=$(bootupctl backend list-bootloaders --component EFI --json)
  [ "${bootloaders_json}" == "${output}" ]
else
  echo "Skip running as not in container"
fi
