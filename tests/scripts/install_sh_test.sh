#!/usr/bin/env bash
# Linux test of scripts/install.sh --release: fake release in a local dir (OUTLOOK_CLI_RELEASE_BASE=file://...),
# fake cmd.exe / wslpath / gh on PATH. Usage: bash tests/scripts/install_sh_test.sh
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
chmod +x "$tmp/bin/"*
rel="$tmp/rel/download/v1.2.3"
printf 'cli\n' >"$rel/outlook-cli.exe"
printf 'index\n' >"$rel/outlook-index.exe"
(cd "$rel" && sha256sum outlook-cli.exe outlook-index.exe >SHA256SUMS)
cp "$rel/SHA256SUMS" "$tmp/good.sums"

# run <expected exit> <args...>: fresh install target each time.
run() {
    local want=$1 got=0
    shift
    rm -rf "${tmp:?}/lad" "${tmp:?}/home"
    mkdir -p "$tmp/home"
    HOME="$tmp/home" PATH="$tmp/bin:$PATH" OUTLOOK_CLI_RELEASE_BASE="file://$tmp/rel" \
        bash "$root/scripts/install.sh" "$@" >"$tmp/out" 2>&1 || got=$?
    [ "$got" = "$want" ] || fail "$* exited $got, expected $want: $(cat "$tmp/out")"
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

if [ "$fails" != 0 ]; then
    echo "$fails failure(s)" >&2
    exit 1
fi
echo "install.sh --release: all checks passed"
