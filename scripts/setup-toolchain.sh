#!/usr/bin/env bash
# Install everything needed to build outlook-cli from Linux/WSL, without root:
#  - Rust (rustup, stable) with the x86_64-pc-windows-gnullvm target
#  - llvm-mingw (Windows cross linker) in ~/opt/llvm-mingw
#  - the linker setting in ~/.cargo/config.toml
set -euo pipefail

if ! command -v rustup >/dev/null 2>&1 && [ ! -x "$HOME/.cargo/bin/rustup" ]; then
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain stable
fi
# shellcheck disable=SC1091
. "$HOME/.cargo/env"
rustup target add x86_64-pc-windows-gnullvm

if [ ! -x "$HOME/opt/llvm-mingw/bin/x86_64-w64-mingw32-clang" ]; then
    mkdir -p "$HOME/opt"
    url=$(curl -fsSL https://api.github.com/repos/mstorsjo/llvm-mingw/releases/latest |
        python3 -c 'import json,sys; d=json.load(sys.stdin); print([a["browser_download_url"] for a in d["assets"] if a["name"].endswith("-ucrt-ubuntu-22.04-x86_64.tar.xz")][0])')
    echo "Downloading $url"
    tmp=$(mktemp -d)
    curl -fsSL "$url" -o "$tmp/llvm-mingw.tar.xz"
    tar -xf "$tmp/llvm-mingw.tar.xz" -C "$HOME/opt"
    rm -rf "$tmp"
    dir=$(find "$HOME/opt" -maxdepth 1 -type d -name 'llvm-mingw-*-ucrt-*' | sort | tail -1)
    ln -sfn "$dir" "$HOME/opt/llvm-mingw"
fi

cfg="$HOME/.cargo/config.toml"
if ! grep -q 'x86_64-pc-windows-gnullvm' "$cfg" 2>/dev/null; then
    cat >>"$cfg" <<CFG

[target.x86_64-pc-windows-gnullvm]
linker = "$HOME/opt/llvm-mingw/bin/x86_64-w64-mingw32-clang"
CFG
fi
rustc --version
echo "Toolchain ready. Build with scripts/build.sh"
