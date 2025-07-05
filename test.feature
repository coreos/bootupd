Feature: Enable bootupd install to aarch64 systems

Scenario: I want to to use bootupd install to install files to /boot/etc on a aarch64 system
Given I have a bootc base image
And I install a dnf package uboot-images-armv8 in the container

# rpm -ql uboot-images-armv8 | grep rpi
# /usr/share/uboot/rpi_arm64
# /usr/share/uboot/rpi_arm64/u-boot.bin

When I run `bootupd extend-payload-to-esp  /usr/lib/uboot/rpi4`
Then I see the content of the file is moved to /usr/lib/efi/(firmware)/%{version}/%{release}/EFI/

When I run bootc install 
Then I see the content of the file is copied over to /boot/efi/
And /boot/bootupd-state.json is updated with the firmware like:
```
{
  "installed": {
    "EFI": {
      "meta": {
        "version": "grub2-2.06,shim-15.8",
        "timestamp": "..."
      },
      "filetree": {
        "children": { ... }
      },
      "firmware": {
        "uboot-images-armv8": {
          "meta": {
            "version": "2024.04-1",
            "timestamp": "..."
          },
          "filetree": {
            "children": {
              "u-boot.bin": {
                "size": 12345,
                "sha512": "..."
              }
            }
          }
        }
      }
    }
  }
}
```