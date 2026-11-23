#!/usr/bin/env bash
# Install outlook-cli.exe and outlook-index.exe for the current Windows user and expose them in WSL:
#   %LOCALAPPDATA%\Programs\outlook-cli\outlook-cli.exe, outlook-index.exe
#   ~/.local/bin/outlook-cli, ~/.local/bin/outlook-index -> those .exe (WSL runs Windows executables natively)
# Usage: scripts/install.sh                      dist/*.exe (built by scripts/build.sh if missing)
#        scripts/install.sh --release vX.Y.Z|latest [--repo owner/name]
#   --release downloads the GitHub Release assets and checks them against SHA256SUMS (fail closed); when `gh` is
#   logged in, `gh attestation verify` must pass too. OUTLOOK_CLI_RELEASE_BASE overrides
#   https://github.com/<repo>/releases (mirrors, tests). Native PowerShell: scripts/install.ps1.
set -euo pipefail
cd "$(dirname "$0")/.."
release='' repo=dmoraine/outlook-cli
usage() { echo "usage: $0 [--release vX.Y.Z|latest] [--repo owner/name]" >&2; exit 2; }
while [ $# -gt 0 ]; do
    case "$1" in
        --release) [ $# -ge 2 ] || usage; release=$2; shift 2 ;;
        --repo) [ $# -ge 2 ] || usage; repo=$2; shift 2 ;;
        *) usage ;;
    esac
done
[[ "$repo" =~ ^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$ ]] || usage
exes=(outlook-cli outlook-index)
src=dist
if [ -n "$release" ]; then
    case "$release" in
        latest) path=latest/download ;;
        v[0-9]*) [[ "$release" =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]] || usage; path="download/$release" ;;
        *) usage ;;
    esac
    base="${OUTLOOK_CLI_RELEASE_BASE:-https://github.com/$repo/releases}"
    src=$(mktemp -d)
    trap 'rm -rf "$src"' EXIT
    for f in SHA256SUMS outlook-cli.exe outlook-index.exe; do
        curl -fsSL --proto '=https,file' --proto-redir '=https' -o "$src/$f" "$base/$path/$f" ||
            { echo "download failed: $f ($release)" >&2; exit 1; }
    done
    # Exactly one line per exe, every hash must match: anything else fails closed.
    for exe in "${exes[@]}"; do
        n=$(grep -cE "^[0-9a-fA-F]{64} [ *]$exe\.exe$" "$src/SHA256SUMS" || true)
        [ "$n" = 1 ] || { echo "SHA256SUMS: expected one line for $exe.exe" >&2; exit 1; }
    done
    (cd "$src" && grep -E '^[0-9a-fA-F]{64} [ *]outlook-(cli|index)\.exe$' SHA256SUMS | sha256sum -c --strict --quiet -) ||
        { echo "SHA-256 mismatch: not installed" >&2; exit 1; }
    attestation=unavailable
    if command -v gh >/dev/null 2>&1 && gh auth status >/dev/null 2>&1; then
        attestation=verified
        for exe in "${exes[@]}"; do
            gh attestation verify "$src/$exe.exe" --repo "$repo" >/dev/null 2>&1 || attestation=failed
        done
    fi
    echo "Release $release: SHA-256 verified, attestation: $attestation"
    [ "$attestation" != failed ] || { echo "attestation failed: not installed" >&2; exit 1; }
else
    [ -f dist/outlook-cli.exe ] && [ -f dist/outlook-index.exe ] || scripts/build.sh
fi
lad=$(cmd.exe /c 'echo %LOCALAPPDATA%' 2>/dev/null | tr -d '\r')
dest="$(wslpath "$lad")/Programs/outlook-cli"
mkdir -p "$dest" "$HOME/.local/bin"
for exe in "${exes[@]}"; do
  cp "$src/$exe.exe" "$dest/"
  ln -sfn "$dest/$exe.exe" "$HOME/.local/bin/$exe"
done
echo "Installed: $lad\\Programs\\outlook-cli\\outlook-cli.exe, outlook-index.exe"
echo "WSL:       $HOME/.local/bin/outlook-cli, $HOME/.local/bin/outlook-index"
echo "PowerShell: & \"\$env:LOCALAPPDATA\\Programs\\outlook-cli\\outlook-cli.exe\" info"
echo "(add that folder to your user PATH to call them as outlook-cli and outlook-index)"
