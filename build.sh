#!/bin/sh
# Build the site into web/: compiles the engine to WebAssembly and generates
# the JavaScript that loads it. Needs the wasm32 target and wasm-bindgen:
#
#   rustup target add wasm32-unknown-unknown
#   cargo install wasm-bindgen-cli --version 0.2.130 --locked
#
# Then serve web/ over HTTPS (or on localhost) and open it.
set -e
cd "$(dirname "$0")"
cargo build --release --lib --target wasm32-unknown-unknown
wasm-bindgen --target web --no-typescript --out-dir web/pkg target/wasm32-unknown-unknown/release/vocsend_web.wasm
ls -l web/pkg
