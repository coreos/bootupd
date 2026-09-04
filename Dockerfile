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
# Install bootc from copr
RUN <<EORUN
set -xeuo pipefail
source /etc/os-release
dnf -y install dnf-plugins-core
if [ "$ID" = "centos" ]; then
  dnf -y copr enable rhcontainerbot/bootc centos-stream-${VERSION_ID}-$(uname -m)
fi
dnf -y install bootc
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

