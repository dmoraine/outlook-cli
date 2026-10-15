#!/usr/bin/env bash
# Install dist/outlook-cli.exe for the current Windows user and expose it in WSL:
#   %LOCALAPPDATA%\Programs\outlook-cli\outlook-cli.exe
#   ~/.local/bin/outlook-cli -> that .exe (WSL runs Windows executables natively)
set -euo pipefail
cd "$(dirname "$0")/.."
[ -f dist/outlook-cli.exe ] || scripts/build.sh
lad=$(cmd.exe /c 'echo %LOCALAPPDATA%' 2>/dev/null | tr -d '\r')
dest="$(wslpath "$lad")/Programs/outlook-cli"
mkdir -p "$dest" "$HOME/.local/bin"
cp dist/outlook-cli.exe "$dest/"
ln -sfn "$dest/outlook-cli.exe" "$HOME/.local/bin/outlook-cli"
echo "Installed: $lad\\Programs\\outlook-cli\\outlook-cli.exe"
echo "WSL:       $HOME/.local/bin/outlook-cli"
echo "PowerShell: & \"\$env:LOCALAPPDATA\\Programs\\outlook-cli\\outlook-cli.exe\" info"
echo "(add that folder to your user PATH to call it as outlook-cli)"
