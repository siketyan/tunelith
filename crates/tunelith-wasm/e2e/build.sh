#!/bin/sh
# Builds tunelith-wasm into ../example/pkg, passing the arguments on to cargo:
# `./build.sh --features mock` for the mock device.
set -eu
cd "$(dirname "$0")"
cargo build --release -p tunelith-wasm --target wasm32-unknown-unknown "$@"
target=$(cargo metadata --format-version 1 --no-deps | node -p 'JSON.parse(require("fs").readFileSync(0)).target_directory')
wasm-bindgen --target web --out-dir ../example/pkg "$target/wasm32-unknown-unknown/release/tunelith_wasm.wasm"
