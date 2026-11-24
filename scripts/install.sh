#!/usr/bin/env bash
# Install outlook-cli.exe and outlook-index.exe for the current Windows user and expose them in WSL:
#   %LOCALAPPDATA%\Programs\outlook-cli\outlook-cli.exe, outlook-index.exe
#   ~/.local/bin/outlook-cli, ~/.local/bin/outlook-index -> those .exe (WSL runs Windows executables natively)
# Usage: scripts/install.sh                      dist/*.exe (built by scripts/build.sh if missing)
#        scripts/install.sh --release vX.Y.Z|latest [--repo owner/name]
#        scripts/install.sh ... --skill hermes|codex|claude|none
#        scripts/install.sh --uninstall
#   --release downloads the GitHub Release assets and checks them against SHA256SUMS (fail closed); when `gh` is
#   logged in, `gh attestation verify` must pass too. OUTLOOK_CLI_RELEASE_BASE overrides
#   https://github.com/<repo>/releases (mirrors, tests). Native PowerShell: scripts/install.ps1.
#   --skill installs the usage skill (.agents/skills/outlook-cli of this checkout) for an agent running in WSL:
#     hermes  copy in <install dir>/skills, added to Hermes `skills.external_dirs` with `hermes config set`
#     codex   copy in $CODEX_HOME/skills (default ~/.codex/skills)
#     claude  copy in ~/.claude/skills
#   Copies carry a marker file; a skill directory without it is never replaced or removed.
#   --uninstall removes the ~/.local/bin links and every managed skill (and its Hermes entry); the exes, user PATH,
#   scheduled task, index and models are the Windows side: scripts/install.ps1 -Uninstall.
set -euo pipefail
cd "$(dirname "$0")/.."
release='' repo=dmoraine/outlook-cli skill=none uninstall=''
usage() {
    echo "usage: $0 [--release vX.Y.Z|latest] [--repo owner/name] [--skill hermes|codex|claude|none] | --uninstall" >&2
    exit 2
}
while [ $# -gt 0 ]; do
    case "$1" in
        --release) [ $# -ge 2 ] || usage; release=$2; shift 2 ;;
        --repo) [ $# -ge 2 ] || usage; repo=$2; shift 2 ;;
        --skill) [ $# -ge 2 ] || usage; skill=$2; shift 2 ;;
        --uninstall) uninstall=1; shift ;;
        *) usage ;;
    esac
done
[[ "$repo" =~ ^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$ ]] || usage
case "$skill" in hermes | codex | claude | none) ;; *) usage ;; esac
[ -z "$uninstall" ] || { [ -z "$release" ] && [ "$skill" = none ]; } || usage
exes=(outlook-cli outlook-index)
skill_src=.agents/skills/outlook-cli
marker=.outlook-cli-managed
lad=$(cmd.exe /c 'echo %LOCALAPPDATA%' 2>/dev/null | tr -d '\r')
dest="$(wslpath "$lad")/Programs/outlook-cli"
codex_skills="${CODEX_HOME:-$HOME/.codex}/skills"
claude_skills="$HOME/.claude/skills"

# Hermes keeps skills.external_dirs as a list; `config get --json` prints it as JSON, which `config set` parses
# back (JSON is YAML). Only our exact entry is added or removed.
json_str() { local s=${1//\\/\\\\}; printf '"%s"' "${s//\"/\\\"}"; }
hermes_dirs() {
    local cur
    cur=$(hermes config get skills.external_dirs --json) || { echo "hermes config get skills.external_dirs failed" >&2; exit 1; }
    case "$cur" in '' | null) cur='[]' ;; \[*\]) ;; *) echo "unexpected skills.external_dirs value" >&2; exit 1 ;; esac
    printf '%s' "$cur"
}
hermes_set() {
    hermes config set skills.external_dirs "$1" >/dev/null || { echo "hermes config set skills.external_dirs failed" >&2; exit 1; }
}
hermes_register() {
    local q cur
    q=$(json_str "$1") cur=$(hermes_dirs)
    case "$cur" in *"$q"*) return 0 ;; esac
    if [ "$cur" = '[]' ]; then hermes_set "[$q]"; else hermes_set "${cur%]}, $q]"; fi
}
hermes_unregister() {
    command -v hermes >/dev/null 2>&1 || return 0
    local q cur new
    q=$(json_str "$1") cur=$(hermes_dirs)
    case "$cur" in *"$q"*) ;; *) return 0 ;; esac
    new=${cur//", $q"/}; new=${new//"$q, "/}; new=${new//"$q"/}
    hermes_set "$new"
}
# copy_skill <skills dir>: (re)places <dir>/outlook-cli, only if absent or made by this script.
copy_skill() {
    local d="$1/outlook-cli"
    if [ -e "$d" ] && [ ! -e "$d/$marker" ]; then
        echo "$d exists and was not installed by this script: left alone" >&2
        exit 1
    fi
    rm -rf "$d"
    mkdir -p "$1"
    cp -R "$skill_src" "$d"
    : >"$d/$marker"
}
drop_skill() { if [ -e "$1/outlook-cli/$marker" ]; then rm -rf "$1/outlook-cli"; fi; }

if [ -n "$uninstall" ]; then
    for exe in "${exes[@]}"; do
        link="$HOME/.local/bin/$exe"
        if [ -L "$link" ] && [ "$(readlink "$link")" = "$dest/$exe.exe" ]; then rm -f "$link"; fi
    done
    hermes_unregister "$dest/skills"
    drop_skill "$dest/skills"
    drop_skill "$codex_skills"
    drop_skill "$claude_skills"
    echo "Removed the WSL links and the managed usage skills; for the exes, PATH, task and data:"
    echo "  powershell.exe -NoProfile -ExecutionPolicy Bypass -File scripts\\install.ps1 -Uninstall"
    exit 0
fi
if [ "$skill" != none ]; then
    [ -f "$skill_src/SKILL.md" ] || { echo "--skill: $skill_src not found (run from a checkout)" >&2; exit 1; }
    if [ "$skill" = hermes ] && ! command -v hermes >/dev/null 2>&1; then
        echo "--skill hermes: hermes not found on PATH" >&2
        exit 1
    fi
fi

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
mkdir -p "$dest" "$HOME/.local/bin"
for exe in "${exes[@]}"; do
  cp "$src/$exe.exe" "$dest/"
  ln -sfn "$dest/$exe.exe" "$HOME/.local/bin/$exe"
done
echo "Installed: $lad\\Programs\\outlook-cli\\outlook-cli.exe, outlook-index.exe"
echo "WSL:       $HOME/.local/bin/outlook-cli, $HOME/.local/bin/outlook-index"
echo "PowerShell: & \"\$env:LOCALAPPDATA\\Programs\\outlook-cli\\outlook-cli.exe\" info"
echo "(add that folder to your user PATH to call them as outlook-cli and outlook-index)"
case "$skill" in
    hermes) copy_skill "$dest/skills"; hermes_register "$dest/skills"; echo "Skill:     $dest/skills/outlook-cli (Hermes skills.external_dirs)" ;;
    codex) copy_skill "$codex_skills"; echo "Skill:     $codex_skills/outlook-cli" ;;
    claude) copy_skill "$claude_skills"; echo "Skill:     $claude_skills/outlook-cli" ;;
esac
