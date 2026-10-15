#!/usr/bin/env bash
# Run the tests and build the Windows executable (and the Linux binary).
# Output: dist/outlook-cli.exe (Windows, also callable from WSL), dist/outlook-cli (Linux).
set -euo pipefail
cd "$(dirname "$0")/.."
# shellcheck disable=SC1091
. "$HOME/.cargo/env"
cargo test --release --quiet
cargo build --release --target x86_64-pc-windows-gnullvm
cargo build --release
mkdir -p dist
cp target/x86_64-pc-windows-gnullvm/release/outlook-cli.exe dist/
cp target/release/outlook-cli dist/
ls -la dist
