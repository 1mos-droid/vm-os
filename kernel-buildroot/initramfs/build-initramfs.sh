#!/bin/sh
# Assembles a minimal, reproducible Zstd-compressed initramfs archive containing
# BusyBox, GPT/Btrfs resize utilities, and kernel-buildroot/initramfs/init.
set -eu

if [ "$#" -lt 2 ]; then
    echo "usage: $0 <target-dir> <binaries-dir>" >&2
    exit 1
fi

TARGET_DIR=$(CDPATH= cd -- "$1" && pwd)
BINARIES_DIR=$(CDPATH= cd -- "$2" && pwd)
SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
HOST_DIR=${HOST_DIR:-$(dirname "$BINARIES_DIR")/host}
OUT_ARCHIVE="$BINARIES_DIR/initramfs.cpio.zst"

# Locate readelf (prefer Buildroot cross-readelf, fallback to host readelf).
READELF=""
for candidate in "$HOST_DIR"/bin/*-readelf "$(command -v readelf || true)"; do
    if [ -n "$candidate" ] && [ -x "$candidate" ]; then
        READELF=$candidate
        break
    fi
done
if [ -z "$READELF" ]; then
    echo "FAIL: readelf not found to resolve initramfs shared library dependencies" >&2
    exit 1
fi

ZSTD="$HOST_DIR/bin/zstd"
if [ ! -x "$ZSTD" ]; then
    ZSTD=$(command -v zstd || true)
fi
if [ -z "$ZSTD" ] || [ ! -x "$ZSTD" ]; then
    echo "FAIL: zstd not found (enable BR2_PACKAGE_HOST_ZSTD)" >&2
    exit 1
fi

WORK=$(mktemp -d "${TMPDIR:-/tmp}/hyperos-initramfs.XXXXXX")
trap 'rm -rf "$WORK"' EXIT
STAGE="$WORK/root"
QUEUE="$WORK/queue"
BATCH="$WORK/batch"
: > "$QUEUE"

mkdir -p \
    "$STAGE/bin" \
    "$STAGE/sbin" \
    "$STAGE/lib" \
    "$STAGE/usr/bin" \
    "$STAGE/usr/sbin" \
    "$STAGE/usr/lib" \
    "$STAGE/dev" \
    "$STAGE/proc" \
    "$STAGE/sys" \
    "$STAGE/mnt/lower" \
    "$STAGE/mnt/rw" \
    "$STAGE/newroot"

install -m 0755 "$SCRIPT_DIR/init" "$STAGE/init"

enqueue_rel() {
    rel=$1
    [ -n "$rel" ] || return 0
    printf '%s\n' "$rel" >> "$QUEUE"
}

find_and_enqueue_tool() {
    name=$1
    for dir in bin sbin usr/bin usr/sbin; do
        if [ -e "$TARGET_DIR/$dir/$name" ] || [ -L "$TARGET_DIR/$dir/$name" ]; then
            enqueue_rel "$dir/$name"
            return 0
        fi
    done
    echo "FAIL: required initramfs binary '$name' not found in $TARGET_DIR" >&2
    exit 1
}

# Seed queue with BusyBox, util-linux mount/partition tools, and btrfs-progs.
for tool in busybox mount umount blkid sfdisk partx btrfs mkfs.btrfs; do
    find_and_enqueue_tool "$tool"
done

# Iteratively copy staged files, symlink targets, PT_INTERP loaders, and NEEDED libraries.
while [ -s "$QUEUE" ]; do
    mv "$QUEUE" "$BATCH"
    : > "$QUEUE"

    while IFS= read -r rel; do
        [ -n "$rel" ] || continue
        src="$TARGET_DIR/$rel"
        dst="$STAGE/$rel"

        if [ ! -e "$src" ] && [ ! -L "$src" ]; then
            continue
        fi
        if [ -e "$dst" ] || [ -L "$dst" ]; then
            continue
        fi

        mkdir -p "$(dirname "$dst")"
        if [ -L "$src" ]; then
            link_target=$(readlink "$src")
            ln -snf "$link_target" "$dst"
            case "$link_target" in
                /*)
                    enqueue_rel "${link_target#/}"
                    ;;
                *)
                    base_dir=$(dirname "$rel")
                    resolved=$(CDPATH= cd -- "$TARGET_DIR/$base_dir" && readlink -f "$(basename "$src")")
                    case "$resolved" in
                        "$TARGET_DIR"/*)
                            enqueue_rel "${resolved#"$TARGET_DIR/"}"
                            ;;
                    esac
                    ;;
            esac
            continue
        fi

        cp -p "$src" "$dst"

        # Extract ELF dynamic linker (PT_INTERP) if present.
        interp=$("$READELF" -l "$src" 2>/dev/null | sed -n 's/.*program interpreter: \[\([^]]*\)\].*/\1/p' | head -n 1 || true)
        if [ -n "$interp" ]; then
            enqueue_rel "${interp#/}"
        fi

        # Extract ELF NEEDED shared libraries.
        for lib in $("$READELF" -d "$src" 2>/dev/null | sed -n 's/.*(NEEDED).*Shared library: \[\([^]]*\)\].*/\1/p'); do
            for libdir in lib usr/lib lib64 usr/lib64; do
                if [ -e "$TARGET_DIR/$libdir/$lib" ] || [ -L "$TARGET_DIR/$libdir/$lib" ]; then
                    enqueue_rel "$libdir/$lib"
                    break
                fi
            done
        done
    done < "$BATCH"
    rm -f "$BATCH"
done

# Populate BusyBox applet symlinks where dedicated binaries were not already staged.
for applet in sh awk basename cat chmod dirname echo grep head mkdir mount mountpoint printf readlink sed sleep switch_root tr umount; do
    if [ ! -e "$STAGE/bin/$applet" ] && [ ! -L "$STAGE/bin/$applet" ]; then
        ln -snf busybox "$STAGE/bin/$applet"
    fi
done

# Pack reproducible sorted cpio archive compressed with zstd.
(
    cd "$STAGE"
    find . -print0 | LC_ALL=C sort -z | cpio --null -o -H newc --owner=0:0 --quiet
) | "$ZSTD" -T0 -19 -f -q -o "$OUT_ARCHIVE"

echo "Built initramfs: $OUT_ARCHIVE ($(wc -c < "$OUT_ARCHIVE") bytes)"
