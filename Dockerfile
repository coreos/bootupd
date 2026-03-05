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
dnf config-manager --set-enabled crb
dnf -y install cargo git openssl-devel ostree-devel libzstd-devel
EORUN

# Build bootc from source with bwrap fix (pre-release)
# TODO: Remove this once a bootc release with the bwrap fix is available
ARG bootc_repo=https://github.com/ckyrouac/bootc
ARG bootc_branch=bwrap-fix
RUN <<EORUN
set -xeuo pipefail
git clone --depth=1 -b "${bootc_branch}" "${bootc_repo}" /bootc-build
EORUN
WORKDIR /bootc-build
RUN --mount=type=cache,target=/bootc-build/target --mount=type=cache,target=/var/roothome <<EORUN
set -xeuo pipefail
cargo build --release --bin bootc
install -D -m 0755 target/release/bootc /out-bootc/usr/bin/bootc
EORUN

# Now copy the bootupd source
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
# Install pre-release bootc with bwrap fix over the base image version
# TODO: Remove this once a bootc release with the bwrap fix is available
COPY --from=build /out-bootc/ /
# Remove /var/roothome as workaround
RUN <<EORUN
set -xeuo pipefail
[ -d /var/roothome ] && rm -rf /var/roothome
EORUN
# Sanity check this too
RUN bootc container lint --fatal-warnings

