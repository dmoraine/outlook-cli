#!/usr/bin/env bash
# Run the tests and build the Windows executables (and the Linux binary).
# Output: dist/outlook-cli.exe (Windows, also callable from WSL), dist/outlook-cli (Linux),
# dist/outlook-index.exe (Windows, preview, cargo feature "index").
set -euo pipefail
cd "$(dirname "$0")/.."
# shellcheck disable=SC1091
. "$HOME/.cargo/env"
# The bundled SQLite (feature "index") is C, compiled with llvm-mingw.
export PATH="$HOME/opt/llvm-mingw/bin:$PATH"
cargo test --release --quiet
cargo test --release --quiet --features index
cargo build --release --target x86_64-pc-windows-gnullvm
cargo build --release --target x86_64-pc-windows-gnullvm --features index --bin outlook-index
cargo build --release
mkdir -p dist
cp target/x86_64-pc-windows-gnullvm/release/outlook-cli.exe dist/
cp target/x86_64-pc-windows-gnullvm/release/outlook-index.exe dist/
cp target/release/outlook-cli dist/
ls -la dist
