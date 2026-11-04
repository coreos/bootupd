# Overall design

The initial focus here is updating the [ESP](https://en.wikipedia.org/wiki/EFI_system_partition), but the overall design of bootupd contains a lot of abstraction to support different "components".

Today there are two: an EFI component that updates shim and the bootloader on
the ESP (x86_64, aarch64, riscv64), and a BIOS component that runs `grub2-install`
(x86_64, ppc64le).

## Ideal case

In the ideal case, an OS builder uses `bootupctl backend install` to install all bootloader data,
and thereafter it is fully (exclusively) managed by bootupd. It would e.g. be an error
for an administrator to manually invoke `grub2-install` again.

In other words, an end user system would simply invoke `bootupctl update` as desired.
In practice this is driven by `bootloader-update.service` on boot.

However, we're not in that ideal case. Thus bootupd has the concept of "adoption" where
we start tracking the installed state as we find it.

## Handling adoption

Historically, Fedora CoreOS created the `EFI/fedora/grub.cfg` file outside of the
OSTree, so no updates for it were included in the OSTree.

This type of problem is exactly what bootupd should be solving, and it now does:
images ship static GRUB configs in `/usr/lib/bootupd/grub2-static`, and
`bootupctl backend install --with-static-configs` writes them to `/boot/grub2`
and the ESP. Existing systems can migrate with
`bootupctl adopt-and-update --with-static-config`.

However, we need to be very cautious in handling this because we basically can't
assume we own all of the state. We shouldn't touch any files that we
don't know about.

## Update decisions

An update is required when the update payload content differs from the installed content.
The saved state stores a filetree, which contains a set of file checksums of what was installed.
The update payload in the booted image provides the same kind of tree.
If the trees differ, bootupd applies the update.

Historically, package versions were used to decide updates, but they
are now only stored and displayed and are not a factor when
deciding if an update is required.

An update payload must carry the components the system boots from, such as the bootloader.
Where the installed content includes shim, the payload must carry it too.
`bootupctl update` refuses a payload which does not, since applying it would remove those
files from the ESP. `bootupctl backend install` also refuses a payload without the
component for the requested bootloader.

## Upgrade edges

We don't necessarily want to update the bootloader data, even if a new update happens to be provided.
For example, Fedora does "mass rebuilds" usually once a release, but it's not strictly necessary
to update users' bootloaders then.

A common policy in fact might be "only update bootloader for security issue or if it's strictly necessary".

A "strictly necessary" upgrade would be one like the GRUB BLS parsing support.

There is no support for upgrade edges in the code.

Updates follow changed content, so an image with a changed payload always triggers an update.
To hold back bootloader updates on a system that has them enabled by default, mask `bootloader-update.service`.
