#!/usr/bin/env bash
# Linux test of scripts/install.sh --release, --skill and --uninstall: fake release in a local dir
# (OUTLOOK_CLI_RELEASE_BASE=file://...), fake cmd.exe / wslpath / gh / hermes on PATH.
# Usage: bash tests/scripts/install_sh_test.sh
set -euo pipefail
root=$(cd "$(dirname "$0")/../.." && pwd)
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
fails=0
fail() { echo "FAIL: $*" >&2; fails=$((fails + 1)); }

mkdir -p "$tmp/bin" "$tmp/rel/download/v1.2.3"
printf '#!/bin/sh\necho "C:\\\\Users\\\\u\\\\AppData\\\\Local\r"\n' >"$tmp/bin/cmd.exe"
printf '#!/bin/sh\necho "%s/lad"\n' "$tmp" >"$tmp/bin/wslpath"
# gh: "auth status" succeeds when FAKE_GH_AUTH=1; "attestation verify" exits FAKE_GH_ATTEST.
cat >"$tmp/bin/gh" <<'GH'
#!/bin/sh
case "$1" in
    auth) [ "${FAKE_GH_AUTH:-0}" = 1 ] ;;
    attestation) exit "${FAKE_GH_ATTEST:-1}" ;;
    *) exit 1 ;;
esac
GH
# hermes: only `config get skills.external_dirs --json` and `config set skills.external_dirs <list>`;
# the list is kept as JSON text in $FAKE_HERMES_DIRS (what the real `config get --json` prints).
cat >"$tmp/bin/hermes" <<'HERMES'
#!/bin/sh
[ "$1 $3" = "config skills.external_dirs" ] || exit 2
case "$2" in
    get) [ "$4" = --json ] && cat "$FAKE_HERMES_DIRS" ;;
    set) printf '%s\n' "$4" >"$FAKE_HERMES_DIRS" && echo "Set skills.external_dirs" ;;
    *) exit 2 ;;
esac
HERMES
chmod +x "$tmp/bin/"*
rel="$tmp/rel/download/v1.2.3"
printf 'cli\n' >"$rel/outlook-cli.exe"
printf 'index\n' >"$rel/outlook-index.exe"
(cd "$rel" && sha256sum outlook-cli.exe outlook-index.exe >SHA256SUMS)
cp "$rel/SHA256SUMS" "$tmp/good.sums"

# go <expected exit> <args...>: runs install.sh on the current install target.
go() {
    local want=$1 got=0
    shift
    HOME="$tmp/home" PATH="$tmp/bin:$PATH" OUTLOOK_CLI_RELEASE_BASE="file://$tmp/rel" \
        FAKE_HERMES_DIRS="$tmp/hermes-dirs" bash "$root/scripts/install.sh" "$@" >"$tmp/out" 2>&1 || got=$?
    [ "$got" = "$want" ] || fail "$* exited $got, expected $want: $(cat "$tmp/out")"
}
# run <expected exit> <args...>: fresh install target each time.
run() {
    rm -rf "${tmp:?}/lad" "${tmp:?}/home"
    mkdir -p "$tmp/home"
    go "$@"
}
installed() { [ -f "$tmp/lad/Programs/outlook-cli/outlook-cli.exe" ] && [ -f "$tmp/lad/Programs/outlook-cli/outlook-index.exe" ]; }
nothing() { [ ! -e "$tmp/lad/Programs/outlook-cli/outlook-cli.exe" ] && [ ! -e "$tmp/lad/Programs/outlook-cli/outlook-index.exe" ]; }

# Good release, gh not logged in: installed, attestation unavailable.
run 0 --release v1.2.3
installed || fail "good release not installed"
[ "$(cat "$tmp/lad/Programs/outlook-cli/outlook-index.exe")" = index ] || fail "wrong outlook-index.exe content"
[ -L "$tmp/home/.local/bin/outlook-index" ] || fail "no ~/.local/bin/outlook-index link"
grep -q 'attestation: unavailable' "$tmp/out" || fail "attestation not reported unavailable: $(cat "$tmp/out")"

# gh logged in, attestation ok / failed (fail closed).
FAKE_GH_AUTH=1 FAKE_GH_ATTEST=0 run 0 --release v1.2.3
grep -q 'attestation: verified' "$tmp/out" || fail "attestation not reported verified"
FAKE_GH_AUTH=1 FAKE_GH_ATTEST=1 run 1 --release v1.2.3
nothing || fail "installed despite failed attestation"
grep -q 'attestation: failed' "$tmp/out" || fail "attestation failure not reported"

# Tampered hash: fail closed, nothing installed.
sed -E "1s/^[0-9a-f]{64}/$(printf '0%.0s' $(seq 64))/" "$tmp/good.sums" >"$rel/SHA256SUMS"
cmp -s "$rel/SHA256SUMS" "$tmp/good.sums" && fail "test setup: hash not tampered"
run 1 --release v1.2.3
nothing || fail "installed despite a hash mismatch"

# Missing entry: fail closed.
grep -v outlook-index "$tmp/good.sums" >"$rel/SHA256SUMS"
run 1 --release v1.2.3
nothing || fail "installed with outlook-index.exe missing from SHA256SUMS"

# Missing SHA256SUMS: fail closed.
rm "$rel/SHA256SUMS"
run 1 --release v1.2.3
nothing || fail "installed without SHA256SUMS"
cp "$tmp/good.sums" "$rel/SHA256SUMS"

# Unknown version / bad arguments.
run 1 --release v9.9.9
nothing || fail "installed a missing version"
run 2 --release 1.2
run 2 --release v1.2.3 --repo 'bad repo'
run 2 --release v1.2.3 --skill cursor

# --skill hermes: copy in the install dir, registered in skills.external_dirs next to an unrelated entry; idempotent.
prog="$tmp/lad/Programs/outlook-cli"
printf '%s\n' '["/srv/team skills"]' >"$tmp/hermes-dirs"
run 0 --release v1.2.3 --skill hermes
[ -f "$prog/skills/outlook-cli/SKILL.md" ] || fail "hermes: skill not copied to the install dir"
[ -f "$prog/skills/outlook-cli/references/outlook-index.md" ] || fail "hermes: references not copied"
want_dirs="[\"/srv/team skills\", \"$prog/skills\"]"
[ "$(cat "$tmp/hermes-dirs")" = "$want_dirs" ] || fail "hermes: external_dirs $(cat "$tmp/hermes-dirs")"
go 0 --release v1.2.3 --skill hermes
[ "$(cat "$tmp/hermes-dirs")" = "$want_dirs" ] || fail "hermes: second install changed external_dirs"

# --skill claude / codex: managed copies in ~/.claude/skills and $CODEX_HOME/skills (default ~/.codex/skills).
go 0 --release v1.2.3 --skill claude
[ -f "$tmp/home/.claude/skills/outlook-cli/SKILL.md" ] || fail "claude: skill not installed"
mkdir -p "$tmp/home/.claude/skills/other"
printf 'mine\n' >"$tmp/home/.claude/skills/other/SKILL.md"
go 0 --release v1.2.3 --skill codex
[ -f "$tmp/home/.codex/skills/outlook-cli/SKILL.md" ] || fail "codex: skill not installed in ~/.codex/skills"
CODEX_HOME="$tmp/codex" go 0 --release v1.2.3 --skill codex
[ -f "$tmp/codex/skills/outlook-cli/SKILL.md" ] || fail "codex: CODEX_HOME not honoured"

# A skill directory not made by install.sh is never replaced or removed.
mkdir -p "$tmp/foreign/skills/outlook-cli"
printf 'mine\n' >"$tmp/foreign/skills/outlook-cli/SKILL.md"
CODEX_HOME="$tmp/foreign" go 1 --release v1.2.3 --skill codex
[ "$(cat "$tmp/foreign/skills/outlook-cli/SKILL.md")" = mine ] || fail "codex: unmanaged skill overwritten"

# --uninstall: links and managed skills gone, unrelated entries, skills and data kept; idempotent.
mkdir -p "$tmp/lad/outlook-cli"
printf 'x\n' >"$tmp/lad/outlook-cli/index.sqlite"
CODEX_HOME="$tmp/codex" go 0 --uninstall
for l in outlook-cli outlook-index; do [ ! -e "$tmp/home/.local/bin/$l" ] || fail "uninstall: $l link left"; done
[ ! -e "$tmp/home/.claude/skills/outlook-cli" ] || fail "uninstall: claude skill left"
[ ! -e "$tmp/codex/skills/outlook-cli" ] || fail "uninstall: \$CODEX_HOME skill left"
[ ! -e "$prog/skills/outlook-cli" ] || fail "uninstall: hermes skill copy left"
[ "$(cat "$tmp/hermes-dirs")" = '["/srv/team skills"]' ] || fail "uninstall: external_dirs $(cat "$tmp/hermes-dirs")"
[ "$(cat "$tmp/home/.claude/skills/other/SKILL.md")" = mine ] || fail "uninstall: unrelated skill removed"
[ "$(cat "$tmp/foreign/skills/outlook-cli/SKILL.md")" = mine ] || fail "uninstall: unmanaged skill removed"
[ -f "$tmp/lad/outlook-cli/index.sqlite" ] || fail "uninstall: data removed"
go 0 --uninstall
[ ! -e "$tmp/home/.codex/skills/outlook-cli" ] || fail "uninstall: ~/.codex skill left"
[ "$(cat "$tmp/hermes-dirs")" = '["/srv/team skills"]' ] || fail "second uninstall changed external_dirs"
# A link that does not point to the install dir is not ours.
mkdir -p "$tmp/home/.local/bin" && ln -s /bin/true "$tmp/home/.local/bin/outlook-cli"
go 0 --uninstall
[ -L "$tmp/home/.local/bin/outlook-cli" ] || fail "uninstall removed a foreign link"
go 2 --uninstall --skill hermes

# --skill hermes without hermes on PATH: refused before anything is installed.
mv "$tmp/bin/hermes" "$tmp/hermes.off"
PATH=/usr/bin:/bin run 1 --release v1.2.3 --skill hermes
nothing || fail "installed although hermes is missing"
mv "$tmp/hermes.off" "$tmp/bin/hermes"

if [ "$fails" != 0 ]; then
    echo "$fails failure(s)" >&2
    exit 1
fi
echo "install.sh --release, --skill, --uninstall: all checks passed"
