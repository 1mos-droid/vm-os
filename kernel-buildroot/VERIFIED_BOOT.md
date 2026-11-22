# Verified boot and A/B update design (B5)

Status: design for review, as required by `docs/REVIEW_FIXES.md` B5. The current
image has neither authenticated boot nor A/B updates. This document does not
change its boot chain or disk layout. Implement this in a separate change after
the signing and disk migration choices below are reviewed.

## Boot chain and trust

Release images use UEFI Secure Boot with an enrolled release certificate and
signed unified kernel images (UKIs). Each UKI contains the kernel, initramfs,
fixed command line, slot identity, and the SHA-256 dm-verity root hash. No
external command line may override the embedded verity parameters. Signing
keys stay outside the repository and ordinary image CI; an isolated signing
job signs a verified build artifact. Test keys are explicitly development-only.

Keep EROFS read-only, but mount it through dm-verity before constructing the
existing volatile OverlayFS. Generate the verity tree from the final EROFS
bytes, after all target hooks. Include the data block count, hash offset, salt,
and root hash in the UKI. Enable built-in device mapper, dm-verity, SHA-256,
and the required signature facilities. Stage `veritysetup` and its libraries in
the initramfs using its existing dependency copier. A missing or invalid tree
must fail closed, without the current interactive root-shell recovery path.
Writable Btrfs guest data is separate and is not authenticated by dm-verity.

BIOS remains a clearly labeled development/recovery path: it can check EROFS
integrity through verity, but cannot authenticate a root hash on mutable boot
media. It cannot provide the UEFI trust guarantee. TPM PCR measurement can
record UKI identity later; it is optional and is not an authentication bypass.

## Slots and activation

Use an ESP, fixed root A and root B partitions (each carrying EROFS plus its
verity tree), then the existing final expandable Btrfs data partition. Give
all partitions distinct stable PARTUUIDs. Each root slot has its matching UKI
in a separate ESP file. Retain the previous complete UKI and root pair until
the new pair has passed boot health checks.

GRUB stores `active_slot`, `pending_slot`, `tries_left` and `last_good_slot`
in a fixed-size environment block on the ESP. A pending slot gets three
attempts. Persist the decremented counter before transferring control; if
that write fails, boot the last-good slot. With zero attempts left, clear the
pending selection and fall back. Only permit selection among signed UKIs;
GRUB must not expose an unsigned kernel, initramfs, editable command line or
unrestricted command shell in the Secure Boot path. Validate GRUB lockdown,
UKI signature enforcement, and environment writes on the supported firmware
before choosing this implementation over direct firmware UKI boot management.

The updater runs locally as root, holds one exclusive update lock, rejects
stateless mode, and resolves the running slot from the authenticated boot
metadata. It verifies a signed release manifest covering version, architecture,
sizes, UKIs, root images and verity parameters before opening any partition
for writing. Never trust a user-supplied device path or write the active slot.
Check target sizes, free ESP space, release compatibility and all digests.

Write and flush the inactive root; verify it by reading back. Write the UKI
to a temporary ESP file, flush and atomically rename it. Only then update and
flush the GRUB pending state. A power loss before activation must leave the
last-good pair bootable. Never reformat, resize or migrate guest data in the
ordinary update transaction. Failures retain the previous pair and produce
an actionable error, without silently marking the update healthy.

An early runit health service checks dm-verity status, mounted data, KSM off,
storage readiness and the daemon socket, then marks the slot good. Graphical
images additionally require a working shell session. A watchdog resets a hung
boot so the attempt counter can reach fallback. Keep manual rollback available
to the last signed, compatible slot; do not claim anti-rollback security without
a reviewed TPM-backed version policy.

## Migration and acceptance

The present disk has one root and data as partition 3. Adding a root slot shifts
data and cannot be an ordinary in-place update. The first A/B release therefore
requires backup, installation on new media, verified guest-data restore and a
documented recovery path. Do not silently repartition deployed disks.

Before shipping: test a modified EROFS block, tree, UKI, signature and command
line; wrong signing keys; truncated payloads; a full ESP; concurrent updates;
every activation power-cut boundary; three unsuccessful boots and fallback;
healthy-slot confirmation; stateless rejection; and guest data surviving both
an update and rollback. Test on UEFI Secure Boot hardware and QEMU/OVMF with
explicit enrollment. BIOS tests must report the weaker integrity-only property.

Review choices: release certificate enrollment/revocation, GRUB's exact locked
UKI chain, root slot capacity, initial migration support, and TPM anti-rollback
policy. No production signing keys or destructive disk migration are supplied
by this design.
