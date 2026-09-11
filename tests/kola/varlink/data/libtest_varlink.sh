# shellcheck shell=bash
# Common checks for varlink tests.

set -xeuo pipefail

# shellcheck disable=SC1091
. "$KOLA_EXT_DATA/libtest.sh"

# Need to write files for parsing output
cd "$(mktemp -d)"

INTERFACE="org.coreos.bootupd1"
SOCKET="/run/bootupd/${INTERFACE}"
SYNC_ENDPOINT="${INTERFACE}.SyncFwupdUpdates"

CAPSULE_DIR="updates"
CAPSULE_FILE1="test.cap"
CAPSULE_FILE2="a.cap"
CAPSULE_FILE3="b.cap"

varlink_sync() {
    varlinkctl call "${SOCKET}" "${SYNC_ENDPOINT}" \
        "{\"partuuid\": \"$1\", \"capsule_dir\": \"${2:-$CAPSULE_DIR}\"}" > out.txt 2>&1
}

check_introspect() {
    varlinkctl introspect "${SOCKET}" "${INTERFACE}" > out.txt 2>&1
    assert_file_has_content out.txt "SyncFwupdUpdates"
    assert_file_has_content out.txt "partuuid"
    assert_file_has_content out.txt "capsule_dir"
    ok "introspection shows interface"
}

check_service() {
    unit="bootupd-varlink.socket"
    if ! systemctl is-enabled "${unit}" 1> /dev/null; then
        # TODO: remove when enabled by default
        systemctl start "$unit"
        # systemctl status "${unit}"
        # fatal "${unit} should be enabled"
    fi
    ok "${unit} is enabled"
}

check_failures() {
    esp_partuuid=$(lsblk -rn -o PARTLABEL,PARTUUID | awk '$1 ~ /^(EFI-SYSTEM|esp-)/ {print $2}' | head -1)
    test -n "${esp_partuuid}" || fatal "ESP has no PARTUUID"

    if varlink_sync "invalid_partuuid" "EFI/test_dir"; then
        fatal "bad partuuid should return error"
    fi
    assert_file_has_content out.txt "No ESP found"
    ok "bad partuuid rejected"

    if varlink_sync "${esp_partuuid}" "EFI/test_dir"; then
        fatal "missing capsule dir should return error"
    fi
    assert_file_has_content out.txt "directory not found"
    ok "missing capsule dir rejected"

    if varlink_sync "${esp_partuuid}" "/EFI/test_dir"; then
        fatal "absolute capsule_dir should be rejected"
    fi
    assert_file_has_content out.txt "relative"
    ok "absolute path for capsule_dir rejected"

    if varlink_sync "${esp_partuuid}" "../parent/"; then
        fatal "capsule_dir with path traversal should be rejected"
    fi
    assert_file_has_content out.txt "path traversal"
    ok "path traversal for capsule_dir rejected"
}

check_basic() {
    check_service
    check_introspect
    check_failures
}

check_no_raid() {
    efipart=/dev/disk/by-partlabel/EFI-SYSTEM
    esp_partuuid=$(lsblk -rn -o PARTUUID "${efipart}" | head -1)
    test -n "${esp_partuuid}" || fatal "ESP has no PARTUUID"

    efi_mount=$(mktemp -d)

    # NO FILES
    mount "${efipart}" "${efi_mount}"
    mkdir -p "${efi_mount}/${CAPSULE_DIR}"
    umount "${efi_mount}"

    if ! varlink_sync "${esp_partuuid}"; then
        cat out.txt
        fatal "failed to sync with no capsule files present"
    fi
    ok "sync with no capsule files"

    # SINGLE FILE
    mount "${efipart}" "${efi_mount}"
    echo "test" > "${efi_mount}/${CAPSULE_DIR}/${CAPSULE_FILE1}"
    umount "${efi_mount}"

    if ! varlink_sync "${esp_partuuid}"; then
        cat out.txt
        fatal "failed to sync with 1 capsule file present"
    fi
    ok "sync with 1 capsule file"

    # MULTIPLE FILES
    mount "${efipart}" "${efi_mount}"
    assert_file_has_content "${efi_mount}/${CAPSULE_DIR}/${CAPSULE_FILE1}" "test"
    echo "test2" > "${efi_mount}/${CAPSULE_DIR}/${CAPSULE_FILE2}"
    echo "test3" > "${efi_mount}/${CAPSULE_DIR}/${CAPSULE_FILE3}"
    umount "${efi_mount}"

    if ! varlink_sync "${esp_partuuid}"; then
        cat out.txt
        fatal "failed to sync with multiple capsule files present"
    fi
    ok "sync with multiple capsule files"

    mount "${efipart}" "${efi_mount}"
    assert_file_has_content "${efi_mount}/${CAPSULE_DIR}/${CAPSULE_FILE1}" "test"
    assert_file_has_content "${efi_mount}/${CAPSULE_DIR}/${CAPSULE_FILE2}" "test2"
    assert_file_has_content "${efi_mount}/${CAPSULE_DIR}/${CAPSULE_FILE3}" "test3"
    umount "${efi_mount}"
}

check_raid() {
    esps=$(lsblk -rn -o PATH,PARTLABEL,PARTUUID | awk '$2 ~ /^esp-/')
    esp1=$(echo "${esps}" | head -1 | cut -d' ' -f1)
    esp2=$(echo "${esps}" | tail -1 | cut -d' ' -f1)
    test -n "${esp1}" || fatal "no primary ESP found"
    test -n "${esp2}" || fatal "no secondary ESP found"
    test "${esp1}" != "${esp2}" || fatal "only found one ESP"

    esp1_partuuid=$(echo "${esps}" | head -1 | cut -d' ' -f3)
    test -n "${esp1_partuuid}" || fatal "ESP has no PARTUUID"

    efi_mount=$(mktemp -d)

    # SINGLE FILE
    mount "${esp1}" "${efi_mount}"
    mkdir -p "${efi_mount}/${CAPSULE_DIR}"
    echo "test" > "${efi_mount}/${CAPSULE_DIR}/${CAPSULE_FILE1}"
    umount "${efi_mount}"

    if ! varlink_sync "${esp1_partuuid}"; then
        cat out.txt
        fatal "failed to sync esp from partuuid ${esp1_partuuid} with a single file"
    fi
    ok "ESP sync of a single file succeeded"

    efi_mount_2=$(mktemp -d)
    mount "${esp2}" "${efi_mount_2}"
    assert_file_has_content "${efi_mount_2}/${CAPSULE_DIR}/${CAPSULE_FILE1}" "test"
    umount "${efi_mount_2}"

    mount "${esp1}" "${efi_mount}"
    assert_file_has_content "${efi_mount}/${CAPSULE_DIR}/${CAPSULE_FILE1}" "test"
    ok "primary ESP unchanged after sync"

    # MULTIPLE FILES
    echo "test2" > "${efi_mount}/${CAPSULE_DIR}/${CAPSULE_FILE2}"
    echo "test3" > "${efi_mount}/${CAPSULE_DIR}/${CAPSULE_FILE3}"
    umount "${efi_mount}"

    if ! varlink_sync "${esp1_partuuid}"; then
        cat out.txt
        fatal "failed to sync esp from partuuid: ${esp1_partuuid}"
    fi
    ok "ESP sync of multiple files succeeded"

    mount "${esp2}" "${efi_mount_2}"
    assert_file_has_content "${efi_mount_2}/${CAPSULE_DIR}/${CAPSULE_FILE1}" "test"
    assert_file_has_content "${efi_mount_2}/${CAPSULE_DIR}/${CAPSULE_FILE2}" "test2"
    assert_file_has_content "${efi_mount_2}/${CAPSULE_DIR}/${CAPSULE_FILE3}" "test3"
    umount "${efi_mount_2}"

    # IDEMPOTENCY
    if ! varlink_sync "${esp1_partuuid}"; then
        cat out.txt
        fatal "failed to sync esp from partuuid: ${esp1_partuuid}"
    fi
    ok "idempotent ESP sync of multiple files succeeded"

    mount "${esp2}" "${efi_mount_2}"
    assert_file_has_content "${efi_mount_2}/${CAPSULE_DIR}/${CAPSULE_FILE1}" "test"
    assert_file_has_content "${efi_mount_2}/${CAPSULE_DIR}/${CAPSULE_FILE2}" "test2"
    assert_file_has_content "${efi_mount_2}/${CAPSULE_DIR}/${CAPSULE_FILE3}" "test3"
    umount "${efi_mount_2}"
    ok "idempotent sync succeeded"
}
