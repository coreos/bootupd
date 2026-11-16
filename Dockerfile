# Build from the current git into a centos-bootc container image.
# Use e.g. --build-arg=base=quay.io/centos-bootc/centos-bootc:stream10
# for CentOS Stream 10, or
# --build-arg=base=quay.io/fedora/fedora-bootc:41 to target Fedora.
#
ARG base=quay.io/centos-bootc/centos-bootc:stream9

FROM $base as build
# This installs our package dependencies, and we want to cache it independently of the rest.
# Basically we don't want changing a .rs file to blow out the cache of packages.
RUN <<EORUN
set -xeuo pipefail
dnf -y install cargo git openssl-devel
EORUN
# Now copy the source
COPY . /build
WORKDIR /build
# See https://www.reddit.com/r/rust/comments/126xeyx/exploring_the_problem_of_faster_cargo_docker/
# We aren't using the full recommendations there, just the simple bits.
RUN --mount=type=cache,target=/build/target --mount=type=cache,target=/var/roothome \
    make && make install-all DESTDIR=/out

FROM $base
# Clean out the default to ensure we're using our updated content
RUN rpm -e bootupd
COPY --from=build /out/ /
# Regenerate the update metadata with this bootupd, as images built with it do.
# Without usr/lib/efi, generate-update-metadata moves the payload out of
# usr/lib/ostree-boot, as the base image already did, so move it back first.
# CentOS Stream 9 keeps the base image's metadata, to keep covering images that
# update bootupd without regenerating it. Fail once the base image ships the
# 0.4.0 format, so that this coverage cannot silently go away.
RUN <<EORUN
set -xeuo pipefail
. /etc/os-release
if [ "$ID" = centos ] && [ "$VERSION_ID" = 9 ]; then
  if [ ! -f /usr/lib/bootupd/updates/EFI.json ]; then
    echo "The base image has no EFI.json" >&2
    exit 1
  fi
  if ! grep -q '"rpm_evr"' /usr/lib/bootupd/updates/EFI.json; then
    echo "The base image's EFI.json now uses the 0.4.0 format, so this no longer tests older metadata" >&2
    exit 1
  fi
  exit 0
fi
if [ ! -d /usr/lib/efi ] && [ -d /usr/lib/bootupd/updates/EFI ]; then
  mkdir -p /usr/lib/ostree-boot/efi
  mv -T /usr/lib/bootupd/updates/EFI /usr/lib/ostree-boot/efi/EFI
fi
bootupctl backend generate-update-metadata
EORUN
# Install bootc from copr
RUN <<EORUN
set -xeuo pipefail
source /etc/os-release
dnf -y install dnf-plugins-core
if [ "$ID" = "centos" ]; then
  dnf -y copr enable rhcontainerbot/bootc centos-stream-${VERSION_ID}-$(uname -m)
fi
dnf -y install bootc
# The copr bootc can be newer than the one in the base image's initramfs;
# a newer bootc with an older initramfs is unsupported (e.g. it can write
# composefs kargs the old initramfs doesn't recognize), so regenerate it.
kver=$(cd /usr/lib/modules && echo *)
dracut --force --kver "$kver" "/usr/lib/modules/$kver/initramfs.img"
dnf clean all
rm -rf /var/log
rm -rf /var/lib
rm -rf /var/cache
rm -rf /run/rhsm
rm -rf /tmp/*
EORUN
# Remove /var/roothome as workaround
RUN <<EORUN
set -xeuo pipefail
[ -d /var/roothome ] && rm -rf /var/roothome
EORUN
# Sanity check this too
RUN bootc container lint --fatal-warnings

