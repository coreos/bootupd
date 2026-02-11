# number: 10
# tmt:
#   summary: Test copy-to-boot on package mode
#   duration: 10m
#
#!/bin/bash
set -eux

echo "Testing copy-to-boot on package mode"

rpm -q bootupd

source /etc/os-release
if [ "$ID" == "fedora" ] && [ "$VERSION_ID" -lt 44 ]; then
    echo "Skip testing on F43 and older"
    exit 0
fi

suffix=""
get_grub_suffix() {
    case "$(uname -m)" in
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
    # assume ESP is already mounted at /boot/efi
    mountpoint /boot/efi
    get_grub_suffix
    grubefi="grub${suffix}.efi"

    grub_source_path=$(find /usr/lib/efi/ -name "${grubefi}")
    if [ -z "${grub_source_path}" ]; then
        echo "Error: Source GRUB binary ${grub_source_path} not found."
        exit 1
    fi

    grub_target_path=/boot/efi/EFI/fedora/${grubefi}
    if [ ! -f "${grub_target_path}" ]; then
        echo "Error: Could not find target GRUB binary ${grub_target_path}."
        exit 1
    fi

    # change grub.efi and it will be synced after copy-to-boot
    echo test > "${grub_target_path}"
    bootupctl backend copy-to-boot

    # get checksum from source /usr/lib/efi/grub2/xx/EFI/fedora/grub.efi
    source_checksum=$(sha256sum "${grub_source_path}" | cut -d' ' -f1)
    # get checksum from target /boot/efi/EFI/fedora/grub.efi
    target_checksum=$(sha256sum "${grub_target_path}" | cut -d' ' -f1)
    # confirm that the target grub.efi is updated
    [ "${source_checksum}" == "${target_checksum}" ]
    tmt-reboot
elif [ "$TMT_REBOOT_COUNT" -eq 1 ]; then
    echo 'After the reboot'
    # just confirm the reboot is successful
    whoami
fi

echo "Run copy-to-boot test successfully"
