# Build from the current git into a c9s-bootc container image.
# Use e.g. --build-arg=base=quay.io/fedora/fedora-bootc:41 to target
# Fedora or another base image instead.
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
dnf -y install dnf-plugins-core
dnf -y copr enable rhcontainerbot/bootc centos-stream-9-x86_64
dnf -y install bootc
dnf clean all
rm -rf /var/log
rm -rf /var/lib
rm -rf /var/cache
rm -rf /run/rhsm
EORUN
# Remove /var/roothome as workaround
RUN <<EORUN
set -xeuo pipefail
rm -rf /var/roothome
EORUN
# Install CI test scripts (used by bcvk ephemeral smoke tests)
COPY --from=build /build/ci/ephemeral-test.sh /usr/libexec/bootupd-tests/ephemeral-test.sh
# Sanity check this too; don't use --fatal-warnings as some base images
# have pre-existing warnings (e.g. /run/systemd content in Fedora).
RUN bootc container lint

