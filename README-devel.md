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

## Continuous Integration

The `ci.yml` and `cross.yml` workflows each end in a `required-checks (<name>)`
sentinel job, as should workflows inherited from [repo-templates](https://github.com/coreos/repo-templates).
The sentinel fails when any job it's watching in that workflow fails.
It's the names of these sentinel jobs which should be included in branch protection.

Matrix jobs report expanded names like `bootc-e2e (ubuntu-24.04, 0, 9)`.
Requiring those names means a rule edit on every matrix change. GitHub
Actions cannot express `needs` across workflows, so each of these workflows
has its own sentinel. Jobs which only have a single stable name, e.g. `tmt-tests`,
can just be required directly.

A sentinel must report on every PR, so its workflow cannot use `pull_request`
path filters. `package-system.yml` keeps its filters and runs only when
`packagesystem/*` or the workflow files change, so it's intentionally not
required. On unrelated PRs its checks can only fail through distro image
drift, which should not block merges.

When jobs are added to `ci.yml` or `cross.yml`, they should also be
added to that workflow's sentinel `needs` list.

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

### Querying which bootloaders an image can install

To find out, without booting a source root, which bootloaders
`backend install --bootloader` can install from its update payload, run:

```
$ bootupctl backend list-bootloaders --src-root /path/to/ostree/deploy --component EFI --json
{"bootloaders":["grub","systemd"],"default-bootloader":null}
```

The answer applies to `backend install` with the same `--component` or
`--auto`, installing the BIOS component to a device. A bootloader is listed
if at least one selected component can install it and, as `install`
requires, every selected component that has update metadata and supports it
can:

- EFI needs exactly one version of the bootloader's component in
  `usr/lib/efi` holding a file with the name shim loads (e.g. `grubx64.efi`),
  and exactly one shim. Without `usr/lib/efi`, it can offer only GRUB, if
  `usr/lib/bootupd/updates/EFI` holds that file.
- BIOS needs `grub2-install` and its modules on the running system, since
  `install` runs them from there.

Warnings say why a component cannot install a bootloader. An unreadable
source root or payload is an error. A real install can still fail because
of its other options or target devices.

If the install only touches the ESP (no device on x86_64, or `--bootloader
systemd` or `grub-cc`), pass `--component EFI` so the BIOS component cannot
affect the answer.

`default-bootloader` is the EFI default set with `backend
set-default-bootloader`, which `install` uses when `--bootloader` is not
given. It is `null` if unset or if EFI is not selected. All names are those
`--bootloader` accepts. Without `--json`, bootloaders are printed one per
line, with the default marked even if it cannot be installed. New JSON keys
may be added; existing keys keep their meaning.
