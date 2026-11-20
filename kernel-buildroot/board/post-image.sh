#!/bin/sh
set -eu
# genimage 18 has no Btrfs backend; create the empty data filesystem first.
truncate -s 512M "$BINARIES_DIR/data.btrfs"
"$HOST_DIR/bin/mkfs.btrfs" -f -L hyperos-data "$BINARIES_DIR/data.btrfs"
exec support/scripts/genimage.sh "$@"
