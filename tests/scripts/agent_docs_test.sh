#!/usr/bin/env bash
# Checks the agent entry points: AGENTS.md / CLAUDE.md, the repo-local skills (.agents/skills), their consent
# rules and every relative Markdown link. Usage: bash tests/scripts/agent_docs_test.sh
# shellcheck disable=SC2016 # backticks in single quotes are Markdown code spans, not command substitutions
set -euo pipefail
cd "$(dirname "$0")/../.."
fails=0
fail() { echo "FAIL: $*" >&2; fails=$((fails + 1)); }

[ "$(wc -l <AGENTS.md)" -le 80 ] || fail "AGENTS.md is longer than 80 lines"
grep -q '](AGENTS.md)' CLAUDE.md || fail "CLAUDE.md does not point to AGENTS.md"
[ "$(grep -cv '^\(@AGENTS.md\|See \[AGENTS.md\](AGENTS.md).\|\)$' CLAUDE.md)" = 0 ] || fail "CLAUDE.md holds more than the pointer"
[ ! -e skills ] || fail "old skills/ directory still present"

# Agent Skills format: front matter with name = directory name and a description.
for f in .agents/skills/*/SKILL.md; do
    d=$(basename "$(dirname "$f")")
    [ "$(sed -n 1p "$f")" = --- ] || fail "$f: no front matter"
    grep -qx "name: $d" "$f" || fail "$f: name is not $d"
    grep -q '^description: ' "$f" || fail "$f: no description"
done
[ -f .agents/skills/outlook-cli/references/outlook-index.md ] || fail "outlook-cli reference missing"
[ -f .agents/skills/outlook-mail-setup/SKILL.md ] || fail "outlook-mail-setup skill missing"

# Consent rules of the usage skill: every gated command is named in the consent rule.
rule=$(grep -A4 "Only after the user's explicit consent in this conversation" .agents/skills/outlook-cli/SKILL.md | tr '\n' ' ')
for c in '`sync`' '`embed`' '`scope` with an option' 'restoring a backup' '`model fetch`' '`bench-embed`' '`doctor`' 'scheduled refresh'; do
    case "$rule" in *"$c"*) ;; *) fail "outlook-cli SKILL.md consent rule misses $c" ;; esac
done
grep -q 'Never set `OUTLOOK_INDEX_DEBUG`' .agents/skills/outlook-cli/SKILL.md || fail "OUTLOOK_INDEX_DEBUG rule missing"
grep -q 'never while another run is active' AGENTS.md || fail "AGENTS.md: no-overlap rule missing"
for c in 'Consent gate 1' 'Consent gate 2' 'Consent gate 3' '## Upgrade' '## Uninstall' '## Errors'; do
    grep -q "$c" .agents/skills/outlook-mail-setup/SKILL.md || fail "outlook-mail-setup misses '$c'"
done
# -Confirm:$false cannot bind under `powershell -File` (5.1): the purge agreement is the -Yes switch.
! grep -rn -- '-Confirm:\$false' AGENTS.md README.md docs .agents || fail "a purge with -Confirm:\$false is documented"
grep -q -- '-PurgeData -Yes' .agents/skills/outlook-mail-setup/SKILL.md || fail "outlook-mail-setup: no -PurgeData -Yes command"
grep -q -- '--no-agent --script' .agents/skills/outlook-mail-setup/references/hermes-cron.md || fail "hermes cron: no script-only job"

# Step 0 is a read-only capability inventory; no optional tool is presented as a requirement.
setup=.agents/skills/outlook-mail-setup/SKILL.md
caps=.agents/skills/outlook-mail-setup/references/capabilities.md
[ -f "$caps" ] || fail "capability reference missing"
grep -q '](references/capabilities.md)' "$setup" || fail "setup skill does not link references/capabilities.md"
grep -q '^## 0\. Capability inventory (read-only' "$setup" || fail "setup step 0 is not a read-only capability inventory"
tool='(gh|git|wsl|pwsh|powershell 7|winget|hermes)'
required="(requires?|needs?|must (have|install))[^.]{0,30}[^a-z0-9_]$tool([^a-z0-9_]|\$)|(^|[^a-z0-9_])$tool[^a-z0-9_][^.]{0,30}(is|are) (required|mandatory)"
for s in 'it requires `gh`.' '`git` is required.' 'WSL is mandatory'; do
    grep -qiE "$required" <<<"$s" || fail "required-tool pattern misses: $s"
done
if grep -inE "$required" "$setup" "$caps" AGENTS.md; then
    fail "setup docs present an optional tool as required"
fi
grep -q 'No extra tool is required' AGENTS.md || fail "AGENTS.md does not say no extra tool is required"
for c in 'gh auth status' 'git --version' '$PSVersionTable' 'WSL_DISTRO_NAME' 'curl.exe --version' 'winget --version' \
    'hermes cron status' 'hermes gateway status' 'Get-ExecutionPolicy -List' 'netsh winhttp show proxy' '-Scope Process'; do
    grep -qF -- "$c" "$caps" || fail "capabilities.md misses probe $c"
done
for c in verified gh_missing gh_not_logged_in failed not_applicable; do
    grep -q "$c" scripts/install.ps1 || fail "install.ps1 help misses attestation_reason $c"
    grep -q "$c" "$setup" || fail "setup skill misses attestation_reason $c"
done
grep -q 'Artifacts' docs/WINDOWS_TESTING.md && grep -q 'release-assets' docs/WINDOWS_TESTING.md ||
    fail "WINDOWS_TESTING.md: no browser download of the dry-run artifact"

# Relative links resolve (anchors and URLs skipped).
for f in AGENTS.md CLAUDE.md README.md docs/*.md .agents/skills/*/SKILL.md .agents/skills/*/references/*.md; do
    while IFS= read -r t; do
        t=${t%%#*}
        case "$t" in '' | http://* | https://* | mailto:*) continue ;; esac
        [ -e "$(dirname "$f")/$t" ] || fail "$f: broken link $t"
    done < <(grep -o '](\([^)]*\))' "$f" | sed 's/^](\(.*\))$/\1/')
done

if [ "$fails" != 0 ]; then
    echo "$fails failure(s)" >&2
    exit 1
fi
echo "agent docs and skills: all checks passed"
