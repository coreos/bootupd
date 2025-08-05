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
else
  echo "Skip running as not in container"
fi
