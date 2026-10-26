#!/bin/sh
# Builds the page into ../src/remote/next/. The paths of this computer are
# left out of the output, so it's the same wherever it's built.
set -e
cd "$(dirname "$0")"
root=$(cd .. && pwd)
cargo_home=${CARGO_HOME:-$HOME/.cargo}
RUSTFLAGS="--remap-path-prefix=$cargo_home=/cargo --remap-path-prefix=$root=/arko" \
    exec trunk build --release "$@"
