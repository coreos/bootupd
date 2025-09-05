#!/bin/bash
set -xeuo pipefail

updates=/usr/lib/bootupd/updates
rm -fv ${updates}/{BIOS,EFI}.json
cp -r ${updates}/EFI /usr/lib/ostree-boot/efi
# prepare /usr/lib/efi/<grub2|shim>/<ver>
if [ ! -d "/usr/lib/efi" ]; then
  arch="$(uname --machine)"
  if [[ "${arch}" == "x86_64" ]]; then
    suffix="x64"
  else
    # Assume aarch64 for now
    suffix="aa64"
  fi

  grub_evr=$(rpm -qa grub2-efi-${suffix} --queryformat '%{EPOCH}:%{VERSION}-%{RELEASE}')
  mkdir -p /usr/lib/efi/grub2/${grub_evr}/EFI/centos
  mv ${updates}/EFI/centos/grub${suffix}.efi /usr/lib/efi/grub2/${grub_evr}/EFI/centos/

  shim_vr=$(rpm -qa shim-${suffix} --queryformat '%{VERSION}-%{RELEASE}')
  mkdir -p /usr/lib/efi/shim/${shim_vr}/EFI/
  mv ${updates}/EFI /usr/lib/efi/shim/${shim_vr}/
else
  rm -rf ${updates}/EFI
fi
bootupctl backend generate-update-metadata -vvv
cat ${updates}/EFI.json | jq
