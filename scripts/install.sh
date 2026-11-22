#!/usr/bin/env bash
# Install dist/outlook-cli.exe and dist/outlook-index.exe for the current Windows user and expose them in WSL:
#   %LOCALAPPDATA%\Programs\outlook-cli\outlook-cli.exe, outlook-index.exe
#   ~/.local/bin/outlook-cli, ~/.local/bin/outlook-index -> those .exe (WSL runs Windows executables natively)
set -euo pipefail
cd "$(dirname "$0")/.."
[ -f dist/outlook-cli.exe ] && [ -f dist/outlook-index.exe ] || scripts/build.sh
lad=$(cmd.exe /c 'echo %LOCALAPPDATA%' 2>/dev/null | tr -d '\r')
dest="$(wslpath "$lad")/Programs/outlook-cli"
mkdir -p "$dest" "$HOME/.local/bin"
for exe in outlook-cli outlook-index; do
  cp "dist/$exe.exe" "$dest/"
  ln -sfn "$dest/$exe.exe" "$HOME/.local/bin/$exe"
done
echo "Installed: $lad\\Programs\\outlook-cli\\outlook-cli.exe, outlook-index.exe"
echo "WSL:       $HOME/.local/bin/outlook-cli, $HOME/.local/bin/outlook-index"
echo "PowerShell: & \"\$env:LOCALAPPDATA\\Programs\\outlook-cli\\outlook-cli.exe\" info"
echo "(add that folder to your user PATH to call them as outlook-cli and outlook-index)"
