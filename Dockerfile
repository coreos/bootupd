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
# Remove /var/roothome as workaround
RUN <<EORUN
set -xeuo pipefail
[ -d /var/roothome ] && rm -rf /var/roothome
EORUN
# Sanity check this too
RUN bootc container lint --fatal-warnings

