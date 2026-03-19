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
    # test with no BIOS.json
    bootupctl remove-component bios
    output_text=$(bootupctl status | tr -d '\r')
    output_json=$(bootupctl status --json)
  fi

  if [ "${arch}" == "x86_64" ] || [ "${arch}" == "aarch64" ]; then
      [ "${components_text_aarch64}" == "${output_text}" ]
      [ "${components_json_aarch64}" == "${output_json}" ]
  fi

  # test with no components
  bootupctl remove-component efi
  output_text=$(bootupctl status | tr -d '\r')
  output_json=$(bootupctl status --json)
  [ -z "${output_text}" ]
  [ "${none_components_json}" == "${output_json}" ]

  # remove none existing component  
  if bootupctl remove-component test 2>err.txt; then
    echo "unexpectedly passed remove none existing component"
    exit 1
  fi
else
  echo "Skip running as not in container"
fi
