#!/bin/sh
# Builds the page into ../src/remote/page/. The paths of this computer are
# left out of the output, so it's the same wherever it's built. With
# erase_components, Leptos erases its views' types, which makes the page
# about a tenth smaller.
set -e
cd "$(dirname "$0")"
root=$(cd .. && pwd)
cargo_home=${CARGO_HOME:-$HOME/.cargo}
RUSTFLAGS="--cfg=erase_components --remap-path-prefix=$cargo_home=/cargo --remap-path-prefix=$root=/arko" \
    exec trunk build --release "$@"
