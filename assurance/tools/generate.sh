#!/bin/sh
# Regenerate the runtime monitor from the FRET requirements.
#
#   requirements/host.json --FRET--> requirements/host.fcs.json
#       --Ogma--> Copilot.hs --Copilot--> vmm-daemon/src/assurance/generated/monitor.{c,h}
#
# Needs: FRET_DIR (a FRET checkout whose fret-electron has npm dependencies),
# `ogma` on PATH, and GHC with the copilot libraries in the default package
# environment. CI (.github/workflows/assurance.yml) pins all three.
set -eu

repo=$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd)
: "${FRET_DIR:?set FRET_DIR to a FRET checkout}"
req="$repo/assurance/requirements/host.json"
fcs="$repo/assurance/requirements/host.fcs.json"
out="$repo/vmm-daemon/src/assurance/generated"

work=$(mktemp -d)
trap 'rm -rf -- "$work"' EXIT

(cd "$FRET_DIR/fret-electron" &&
    NODE_ENV=production node -r @babel/register "$repo/assurance/tools/formalize.js" "$req" "$fcs")

ogma standalone --input-file "$fcs" --input-format fcs --prop-format smv \
    --target-dir "$work/copilot" --target-file-name monitor

(cd "$work/copilot" && runhaskell Copilot.hs)

mkdir -p "$out"
for f in monitor.c monitor.h monitor_types.h; do
    cp "$work/copilot/$f" "$out/$f"
done
cp "$work/copilot/Copilot.hs" "$out/Copilot.hs"
echo "monitor regenerated in $out"
