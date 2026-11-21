#!/bin/sh
set -eu
target=$1
cp "$target/lib/grub/i386-pc/boot.img" "$BINARIES_DIR/boot.img"
mkdir -p "$target/var/lib/hyperos"
# Supervisors need writable state; keep immutable service definitions in /etc/sv.
for service in zram ksm storage vmm-daemon; do
    ln -snf "/run/runit/$service" "$target/etc/sv/$service/supervise"
done
for marker in stopit reboot; do
    ln -snf "/run/runit/$marker" "$target/etc/runit/$marker"
done
