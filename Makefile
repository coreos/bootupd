DESTDIR ?=
PREFIX ?= /usr
LIBEXECDIR ?= ${PREFIX}/libexec
RELEASE ?= 1
CONTAINER_RUNTIME ?= podman
IMAGE_PREFIX ?=
IMAGE_NAME ?= bootupd-build
PACKAGESYSTEM ?= rpm

ifeq ($(RELEASE),1)
        PROFILE ?= release
        CARGO_ARGS = --release
else
        PROFILE ?= debug
        CARGO_ARGS =
endif

ifeq ($(CONTAINER_RUNTIME), podman)
        IMAGE_PREFIX = localhost/
endif

.PHONY: all
all:
	cargo build ${CARGO_ARGS}
	cd target/${PROFILE} && ln -sf bootupd bootupctl

.PHONY: install
install: query-file-$(PACKAGESYSTEM)
	mkdir -p "${DESTDIR}$(PREFIX)/bin" "${DESTDIR}$(LIBEXECDIR)"
	install -D -t "${DESTDIR}$(LIBEXECDIR)" target/${PROFILE}/bootupd
	cd "${DESTDIR}$(PREFIX)/bin" && ln -sf ../libexec/bootupd bootupctl

.PHONY: query-file-$(PACKAGESYSTEM)
query-file-$(PACKAGESYSTEM):
	install -D -m 755 \
		"packagesystem/query-file-owner-$(PACKAGESYSTEM)" \
		"${DESTDIR}$(PREFIX)/lib/bootupd/packagesystem/query-file-owner"

.PHONY: install-grub-static
install-grub-static:
	install -m 644 -D -t ${DESTDIR}$(PREFIX)/lib/bootupd/grub2-static src/grub2/*.cfg
	install -m 644 -D -t ${DESTDIR}$(PREFIX)/lib/bootupd/grub2-static/configs.d src/grub2/configs.d/*.cfg

.PHONY: install-systemd-unit
install-systemd-unit:
	install -m 644 -D -t "${DESTDIR}$(PREFIX)/lib/systemd/system/" systemd/bootloader-update.service

.PHONY: install-all
install-all: install install-grub-static install-systemd-unit
