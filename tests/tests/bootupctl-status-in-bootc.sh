#!/bin/bash
set -xeuo pipefail

# Verify that bootupctl status running in bootc container
if [ ! -d "/sysroot/ostree/repo/" ]; then
  echo "Error: should run test in bootc container"
  exit 100
fi

components_text_x86_64='Available components: BIOS EFI'
components_json_x86_64='{"components":["BIOS","EFI"]}'

components_text_aarch64='Available components: EFI'
components_json_aarch64='{"components":["EFI"]}'

none_components_json='{"components":[]}'

# check if running in container
if [ "$container" ] || [ -f /run/.containerenv ] || [ -f /.dockerenv ]; then
  arch="$(uname --machine)"
  output_text=$(bootupctl status | tr -d '\r')
  output_json=$(bootupctl status --json)

  if [ "${arch}" == "x86_64" ]; then
    [ "${components_text_x86_64}" == "${output_text}" ]
    [ "${components_json_x86_64}" == "${output_json}" ]
    # test if BIOS.json is missing
    mv /usr/lib/bootupd/updates/BIOS.json{,-bak}
    output_text=$(bootupctl status | tr -d '\r')
    output_json=$(bootupctl status --json)
  fi

  if [ "${arch}" == "x86_64" ] || [ "${arch}" == "aarch64" ]; then
      [ "${components_text_aarch64}" == "${output_text}" ]
      [ "${components_json_aarch64}" == "${output_json}" ]
  fi

  # test if no components
  mv /usr/lib/bootupd/updates/EFI.json{,-bak}
  output_text=$(bootupctl status | tr -d '\r')
  output_json=$(bootupctl status --json)
  [ -z "${output_text}" ]
  [ "${none_components_json}" == "${output_json}" ]

else
  echo "Skip running as not in container"
fi
