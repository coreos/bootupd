# Developing bootupd

Currently the focus is Fedora CoreOS.

You can use the normal Rust tools to build and run the unit tests:

`cargo build` and `cargo test`

For real e2e testing, use e.g.
```
export COSA_DIR=/path/to/fcos
cosa build-fast
kola run -E $(pwd) --qemu-image fastbuild-fedora-coreos-bootupd-qemu.qcow2  --qemu-firmware uefi ext.bootupd.*
```

See also [the coreos-assembler docs](https://coreos.github.io/coreos-assembler/working/#using-overrides).

## Building With Containers

There's a reference [Dockerfile](Dockerfile) that builds on [CentOS Stream bootc](https://docs.fedoraproject.org/en-US/bootc/).

## Integrating bootupd into a distribution/OS

Today, bootupd only really works on systems that use ostree.

Many bootupd developers (and current CI flows) target Fedora CoreOS
and derivatives, so it can be used as a "reference" for integration.

There's three parts to integration:

### Modifying query-file-owner

This script provides package ownership detection for `packagesystem.rs`, there are already done scripts for the package systems in `packagesystem/query-file-owner-*`,
in the file filesystem there's only one script for one package system in `usr/lib/bootupd/packagesystem/query-file-owner` in the sysroot.

Now you need to modify this file to work with your package manager, ex. `dpkg/pacman/apk/rpm`

You can test it like this:
`query-file-owner [FILE] [FILE...]`

- Make sure it returns a list of packages info and it should look like this:
  `grub2-efi-x64 1:2.06-95.fc38
  shim-x64 15.6-2` (one package per line)

  So, the output of package looks like this:
  `NAME VERSION`

- You can use already done query-file-owner using for ex.: `make install-all PACKAGESYSTEM=rpm` the supported package systems are: `dpkg`, `rpm`, `pacman` and `apk`

### Generating an update payload

Bootupd's concept of an "update payload" needs to be generated as
part of an OS image (e.g. ostree commit).  
A good reference for this is 
https://github.com/coreos/fedora-coreos-config/blob/88af117d1d2c5e828e5e039adfa03c7cc66fc733/manifests/bootupd.yaml#L12

Specifically, you'll need to invoke
`bootupctl backend generate-update-metadata /` as part of update payload generation.
This scrapes metadata (e.g. RPM versions) about shim/grub and puts them along with
their component files in `/usr/lib/bootupd/updates/`.

### Installing to generated disk images

In order to correctly manage updates, bootupd also needs to be responsible
for laying out files in initial disk images.  A good reference for this is
https://github.com/coreos/coreos-assembler/blob/93efb63dcbd63dc04a782e2c6c617ae0cd4a51c8/src/create_disk.sh#L401

Specifically, you'll need to invoke
`/usr/bin/bootupctl backend install --src-root /path/to/ostree/deploy /sysroot`
where the first path is an ostree deployment root, and the second is the physical
root partition.

This will e.g. inject the initial files into the mounted EFI system partition.
