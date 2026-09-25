#!/bin/sh
# Build the wasm echo artifact and embed it as pure base64 for the prism
# gateway (the carrier's binary-source form; actors.rs include_str!'s
# the file — the content IS the string, no Rust quoting inside).
set -eu
cd "$(dirname "$0")"
cargo build --release --target wasm32-unknown-unknown
printf '%s' "$(base64 -w0 target/wasm32-unknown-unknown/release/echo_wasm.wasm)" > echo_wasm.b64
wc -c echo_wasm.b64
