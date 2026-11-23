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
# -PurgeData -Confirm:$false cannot bind under `powershell -File` (5.1): no purge command documented until F7.
! grep -rn -- '-Confirm:\$false' AGENTS.md README.md docs .agents || fail "a purge with -Confirm:\$false is documented"
grep -q -- '--no-agent --script' .agents/skills/outlook-mail-setup/references/hermes-cron.md || fail "hermes cron: no script-only job"

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
