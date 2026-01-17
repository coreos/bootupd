# number: 10
# tmt:
#   summary: Test posttrans on package mode
#   duration: 10m
#
#!/bin/bash
set -eux

echo "Testing posttrans on package mode"

bootupctl --version

source /etc/os-release
if [ $ID == "fedora" ] && [ $VERSION_ID < 44 ]; then
    echo "Skip on testing on F43 and older"
    exit 0
fi

suffix=""
get_grub_suffix() {
    case $(arch) in
        x86_64)
            suffix="x64"
            ;;
        aarch64)
            suffix="aa64"
            ;;
        *)
            echo "Unsupported arch"
            exit 1
            ;;
    esac
}

if [ "$TMT_REBOOT_COUNT" -eq 0 ]; then
    echo 'Before first reboot'
    get_grub_suffix
    grubefi=grub${suffix}.efi
    grub_path=$(find /usr/lib/efi/ -name ${grub_file})
    if [ -z "$grub_path" ]; then
        echo "Error: Source GRUB binary not found."
        exit 1
    fi
    source_checksum=$(sha256sum ${grub_path} | cut -d' ' -f1)
    # check grubx64.efi is updated
    target_checksum=$(sha256sum /boot/efi/EFI/fedora/${grub_file} | cut -d' ' -f1)
    [ ${source_checksum} == ${target_checksum} ]
    tmt-reboot
elif [ "$TMT_REBOOT_COUNT" -eq 1 ]; then
    echo 'After the reboot'
    # just confirm the reboot is successful
    whoami    
fi

echo "Run posttrans test successfully"
