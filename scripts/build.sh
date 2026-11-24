#!/usr/bin/env bash
# Run the tests and build the Windows executables (and the Linux binary).
# Output: dist/outlook-cli.exe (Windows, also callable from WSL), dist/outlook-cli (Linux),
# dist/outlook-index.exe (Windows, preview, cargo feature "index").
# Usage: scripts/build.sh [--no-tests]   (or SKIP_TESTS=1): build only, tests left to CI.
# CARGO_BUILD_JOBS defaults to 2 to bound RAM; set CARGO_BUILD_JOBS=1 if the build still runs out of memory.
set -euo pipefail
skip_tests=0
[ "${SKIP_TESTS:-}" = 1 ] && skip_tests=1
for arg in "$@"; do
    case "$arg" in
        --no-tests) skip_tests=1 ;;
        *) echo "usage: $0 [--no-tests]" >&2; exit 2 ;;
    esac
done
export CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-2}"
cd "$(dirname "$0")/.."
# shellcheck disable=SC1091
. "$HOME/.cargo/env"
# The bundled SQLite (feature "index") is C, compiled with llvm-mingw.
export PATH="$HOME/opt/llvm-mingw/bin:$PATH"
if [ "$skip_tests" = 1 ]; then
    echo "Tests skipped (--no-tests / SKIP_TESTS=1): CI runs them (.github/workflows/ci.yml)."
else
    # Tests only: no LTO and parallel codegen use far less RAM than the release profile (fat LTO, 1 unit).
    CARGO_PROFILE_RELEASE_LTO=off CARGO_PROFILE_RELEASE_CODEGEN_UNITS=16 cargo test --release --quiet
    CARGO_PROFILE_RELEASE_LTO=off CARGO_PROFILE_RELEASE_CODEGEN_UNITS=16 cargo test --release --quiet --features index
fi
cargo build --release --target x86_64-pc-windows-gnullvm
cargo build --release --target x86_64-pc-windows-gnullvm --features index --bin outlook-index
cargo build --release
mkdir -p dist
cp target/x86_64-pc-windows-gnullvm/release/outlook-cli.exe dist/
cp target/x86_64-pc-windows-gnullvm/release/outlook-index.exe dist/
cp target/release/outlook-cli dist/
ls -la dist
