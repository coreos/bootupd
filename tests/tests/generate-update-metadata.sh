#!/bin/bash
set -xeuo pipefail

updates=/usr/lib/bootupd/updates
rm -fv ${updates}/{BIOS,EFI}.json
if [ -d "/usr/lib/efi" ]; then
  rm -rfv ${updates}/EFI
else
  mv ${updates}/EFI /usr/lib/ostree-boot/efi
fi
# Run generate-update-metadata
bootupctl backend generate-update-metadata -vvv
cat ${updates}/EFI.json | jq

# Verify the bootupd EFI has more than one component installed
version=$(cat ${updates}/EFI.json | jq -r .version | tr ',' ' ')
array=($version)
[ ${#array[*]} -gt 1 ]

[ $(cat ${updates}/EFI.json | jq '.versions | length') -gt 1 ]
