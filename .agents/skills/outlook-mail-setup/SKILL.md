---
name: outlook-mail-setup
description: >-
  Install and set up outlook-cli and outlook-index on the user's Windows PC (native PowerShell, WSL, or Hermes on
  either), build the first local mail index with the user's consent at each costly step, back it up, schedule its
  refresh, install the outlook-cli usage skill for use outside the repository, and handle upgrades and uninstall.
  Use when the user asks to install, set up, index, upgrade or remove outlook-cli / outlook-index. For searching or
  reading mail once installed, use the outlook-cli skill.
---

# outlook-mail-setup

End-to-end onboarding run by the agent from a checkout of `https://github.com/dmoraine/outlook-cli`. Everything
runs on the user's PC, per user, no admin. The only network access: the GitHub release download (or the toolchain
for a source build) and the embedding model download. Read [AGENTS.md](../../../AGENTS.md) first.

**Rules.** Ask before each consent gate and wait for a clear yes in this conversation (or a standing instruction
the user recorded); say what it costs (download size, hours of CPU, disk). Never start a writer (`sync`, `embed`,
`scope` with an option) while another run is active (`outlook-index status` → `running` not `null` and not
`stale`; a scheduled sync). Never paste mailbox content (subjects, names, addresses, folder or file names,
excerpts) into a report or a log; the OST file name often is the user's e-mail address: report its size only.
Never set `OUTLOOK_INDEX_DEBUG`. Answer in the user's language.

## 0. Capability inventory (read-only, no consent needed)

Run the probes of [references/capabilities.md](references/capabilities.md) before proposing anything: shell and
PowerShell editions, execution policy, `git`, `gh` and its login, `winget`, `curl.exe`, WSL, Hermes gateway and
cron scheduler, proxy, TLS inspection, network reachability, CPUs, RAM, disk. They change nothing. Pick the path
from its decision table and tell the user which one and why. Windows PowerShell 5.1 and `curl.exe` cover the
default path; `gh`, `git`, WSL, PowerShell 7, `winget` and Hermes are optional extras, never prerequisites: do not
ask the user to install one unless they want the path it opens.

| Where the agent's shell runs | Detect | Use |
|---|---|---|
| Native Windows PowerShell 5.1 or 7 | `$PSVersionTable.PSVersion`, `$env:OS -eq 'Windows_NT'` | `scripts\*.ps1` via `powershell -NoProfile -ExecutionPolicy Bypass -File …` (process-only policy, no admin) |
| WSL | `$WSL_DISTRO_NAME` set, `cmd.exe` reachable | `scripts/*.sh`; Windows-side steps via `powershell.exe -NoProfile -ExecutionPolicy Bypass -File "$(wslpath -w scripts/<x>.ps1)"` |
| Linux or macOS without Windows | neither | stop: the tools read Outlook's file on a Windows PC |

Agent: Hermes (`hermes --version`; its home = parent of `hermes config path`, default `%LOCALAPPDATA%\hermes` on
Windows, `~/.hermes` elsewhere), Codex, Claude Code, other. Hermes in WSL and the tools on Windows is fine: the
`.exe` runs from WSL through `~/.local/bin`. The commands below are PowerShell; WSL equivalents are noted.

## 1. Checks (read-only, no consent needed)

```powershell
$o = "$env:LOCALAPPDATA\Microsoft\Outlook"
Get-ChildItem $o -File -Include *.ost, *.pst -Recurse -ErrorAction SilentlyContinue |
    Select-Object Extension, @{n = 'GB'; e = { [math]::Round($_.Length / 1GB, 2) } }, LastWriteTime   # no names
Get-Command outlook-cli, outlook-index -ErrorAction SilentlyContinue | Select-Object Name, Version
```

- No `.ost`/`.pst` there: ask where the data file is (PST files often sit in `Documents\Outlook Files`); later
  commands then take `-f <file>` or `OUTLOOK_CLI_FILE`. ANSI PST (Outlook 97–2002) is not supported.
- Existing install: `outlook-index --version`, `outlook-index status --pretty` (index present? `last_sync`,
  `running`, `schema.upgrade_pending`), `outlook-index model list --pretty`, `scripts\schedule.ps1 -Status -Json`,
  `hermes cron list` → go to Upgrade, or resume where it stopped.
- Disk: exes ~5 MB; e5-base model ~1.1 GB (e5-small ~490 MB); index: text + full-text index + vectors (README:
  ~2 MB per 1,000 e5-small vectors, e5-base vectors are twice as large); each backup is a full copy. Source build:
  3–5 GB, about 8 GB free RAM. The index stays in `%LOCALAPPDATA%\outlook-cli` (not under OneDrive).

## 2. Install

Default: the **GitHub release**, checked against `SHA256SUMS` (fail closed) and, when `gh` is logged in, its
artifact attestation. Add `-Skill hermes|codex|claude` (step 8) now if the agent is known.

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File scripts\install.ps1 -Version latest -Json   # or -Version vX.Y.Z
# WSL: scripts/install.sh --release latest
```

Check the JSON: `ok`, `sha256: verified`, `attestation: verified|unavailable`, `versions`, and
`attestation_reason`: `verified`, `gh_missing` or `gh_not_logged_in` (`unavailable`: SHA-256 alone gated the
install, which is fine), `failed` (`ok: false`, nothing installed), `not_applicable` (`-FromDir`). It installs into
`%LOCALAPPDATA%\Programs\outlook-cli\` and adds it to the user PATH for new shells. In a shell opened before:
`Set-Alias outlook-index "$env:LOCALAPPDATA\Programs\outlook-cli\outlook-index.exe"` (same for `outlook-cli`).

**Build from source instead** — and tell the user why — when: no release exists for the wanted version (download
fails with 404: no release published yet), the PC is offline, this is a development checkout, the user's security
policy requires it, or verification failed (never run a file that failed SHA-256 or attestation). The build
downloads rustup, a Rust toolchain and llvm-mingw (~190 MB zip) for the user only: ask first.

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File scripts\build.ps1 -InstallToolchain -NoTests -Json   # then installs
# less RAM: add -Jobs 1. WSL: scripts/setup-toolchain.sh && scripts/build.sh --no-tests && scripts/install.sh
```

## 3. Diagnostic and time estimate

**Consent**: `doctor` reads the whole mailbox file locally (minutes; aggregates only, nothing leaves the PC).

```powershell
outlook-index doctor --pretty                    # optional: doctor --attachments --pretty (300 attachments)
```

Use: `messages.total`, `file.bytes`, `machine.cpus`, `machine.available_ram_bytes`,
`machine.defaults.workers` (e5-base / e5-small), `sample.body_chunks.mailbox_estimate`, and with `--attachments`
`attachments.chunks.mailbox_estimate` (upper bound: at most 30 chunks per attachment content are embedded).

**Estimate** = chunks ÷ rate. Reference rate: **≈ 1.6–2.1 passages/s with e5-base** on the owner's 14-CPU laptop
(e5-small: ≈ 8.2/s there, 4 workers × 3 threads); another PC can differ a lot. For this PC's real rate, offer
`bench-embed` (**consent**: ~30 s of full CPU, synthetic text): `outlook-index bench-embed --pretty` →
`passages_per_s`. Example: 60,000 chunks at 1.6–2.1/s ≈ 8–10.5 h of embedding, plus the indexing and attachment
pass (not measured; `status` shows its rate once it runs). Too long? Offer e5-small, or a narrower embedding scope
(`scope --embed-since …`, consent): keyword search still covers everything.

## 4. Consent gate 1 — model download

"Download the e5-base model (~1.1 GB from huggingface.co, checked by SHA-256, resumable)?"

```powershell
outlook-index model fetch                        # --model e5-small: ~490 MB
outlook-index model list --pretty                # present: true for each file
```

## 5. Consent gate 2 — first sync

Tell the user: hours of CPU at low priority (the PC stays usable), keep it **plugged in** and **sleep disabled
while plugged in** (Settings > System > Power; the user changes it), Outlook can stay open, interruption is safe
(resumable). Ask once for the whole first sync including its resumptions. Start it in the background:

```powershell
$l = "$env:LOCALAPPDATA\outlook-cli\logs"; New-Item -ItemType Directory -Force $l | Out-Null
Start-Process -FilePath "$env:LOCALAPPDATA\Programs\outlook-cli\outlook-index.exe" -ArgumentList 'sync', '--progress', 'jsonl' -WindowStyle Hidden `
    -RedirectStandardOutput "$l\first-sync.json" -RedirectStandardError "$l\first-sync.progress.jsonl"
```

WSL: run that block through `powershell.exe -NoProfile -Command '…'` (it outlives the WSL shell), or
`nohup outlook-index sync --progress jsonl >first-sync.json 2>first-sync.progress.jsonl &`.

Follow it with `outlook-index status --pretty` every few minutes (`running.phase`, `percent`, `rate`, `eta_s`;
`status --watch 60` blocks until the end). Phases: `scan`, `index`, `attachments`, `embed_bodies`,
`embed_attachments`. Done when `running` is `null` and `first-sync.json` holds the summary with `"complete": true`.
`stale: true`, a killed process, a reboot or `--max-time`: start the same command again; it resumes. If `running`
is still `null` a minute after the start, the agent's terminal probably killed the child: give the user the
command to run in their own PowerShell window.

## 6. Backup

`outlook-index backup --pretty` (safe, also during a sync). Report `integrity`, `schema_version`, `counts`,
`bytes`, not `destination`.

## 7. Consent gate 3 — periodic refresh

Offer one of (never two: they would collide on the sync lock):
- **Windows scheduled task** (per user, no admin, AC power only, no overlap, low priority, JSON logs, 14 kept):
  `scripts\schedule.ps1 -Every 1h|4h|daily -At HH:mm -Json`; `-Status -Json`; `-Remove -Json`.
- **Hermes cron job** (script only, no LLM): [references/hermes-cron.md](references/hermes-cron.md).
- **None**: the user asks for a sync when needed (`outlook-cli list --since <last_sync.at>` covers recent mail).

## 8. Usage skill outside the repository

Inside a trusted checkout Hermes already loads `.agents/skills` (`hermes skills trust <checkout>`). For use
everywhere, rerun the install of step 2 (same source and version) with `-Skill hermes|codex|claude`
(WSL: `--skill`); not while a sync runs. hermes: copy in the install folder, registered in Hermes
`skills.external_dirs` with `hermes config set`; codex: `$CODEX_HOME\skills` (default `~\.codex\skills`); claude:
`~\.claude\skills`. Idempotent; a same-named skill not installed by the script is left alone (error).

## 9. Report to the user (counts only)

Versions; install source (release + SHA-256/attestation result, or source build + reason); messages, file size,
CPUs, RAM; estimate vs actual time; sync summary counts (`new`, `chunks_added`, `attachments_extracted`,
`attachments_skipped_by_reason`, `embedding.embedded`); `status` totals (`messages`, `chunks`, vectors,
`db_bytes`); backup `integrity`; refresh choice and next run; skill installed for which agent. No paths, names,
subjects or excerpts.

## Upgrade

1. `outlook-index status` → no run; pause the refresh: `Disable-ScheduledTask -TaskName 'outlook-index sync'`
   (or `schedule.ps1 -Remove`), `hermes cron pause <job_id>`.
2. `outlook-index backup --pretty`.
3. Install the new version as in step 2 (same `-Skill`; release, or `git pull` + source build).
4. `outlook-index status`: `schema.upgrade_pending: true` is normal; the next `sync` migrates (consent; short unless
   new chunks must be embedded).
5. Resume the refresh: `schedule.ps1 -Every … -At …` again (refreshes its copy of the script) or
   `Enable-ScheduledTask`, `hermes cron resume <job_id>`.

## Uninstall

Ask first: **keep the data** (index, models, backups, logs in `%LOCALAPPDATA%\outlook-cli`: a reinstall resumes)
or **delete it** (irreversible; the index holds a copy of the mail).

1. No run active. Hermes cron: `hermes cron remove <job_id>`, delete its script from `<hermes home>\scripts\`.
2. WSL: `scripts/install.sh --uninstall` (links, WSL-side skills and their Hermes entry).
3. `powershell -NoProfile -ExecutionPolicy Bypass -File scripts\install.ps1 -Uninstall -Json`: exes, user PATH entry,
   scheduled task, managed skills and their Hermes entry. The data stays. Only if the user chose **delete it**
   in this conversation, run instead (`data_purged: true`; `-PurgeData` without `-Yes` is refused):
   `powershell -NoProfile -ExecutionPolicy Bypass -File scripts\install.ps1 -Uninstall -PurgeData -Yes -Json`
4. Optional, if the user wants: `hermes skills untrust <checkout>`; a toolchain installed by `build.ps1` lives in
   `%LOCALAPPDATA%\outlook-cli\toolchain` (purged with the data) and rustup in `%USERPROFILE%\.cargo`
   (`rustup self uninstall`, only if nothing else uses it).

## Errors

| Symptom | Action |
|---|---|
| `install.ps1` `ok: false`, SHA-256 mismatch or `attestation failed` | stop, run nothing downloaded, tell the user; offer a source build (security reason) |
| release download fails (404, offline, proxy) | no published release or no network: source build, reason stated |
| script blocked by execution policy | use `powershell -NoProfile -ExecutionPolicy Bypass -File …`; if a group policy forbids it, tell the user |
| "running from the install dir" | a sync is running: wait (`status`), never kill it without the user |
| `build.ps1` "missing: …" | ask, then `-InstallToolchain`; out of memory: `-Jobs 1` |
| exit 4 `io` | file not found or locked: check the data file, `-f`; always the Windows `.exe` |
| exit 5 `format` | unsupported file (ANSI PST, not an OST/PST) or index newer than the tool: report |
| exit 2 "another sync is running" | wait for the run (or the scheduled one) to end |
| exit 2 mass deletion refused | report; `--allow-mass-delete` only after the user checked and agreed |
| `model fetch` fails | network or proxy; rerun resumes; or manual download + `--model-dir` (README) |
| exit 3 `upgrade_pending` | a `sync` migrates (consent) |
| `permissions_warning`, `onedrive` warning | report; do not change permissions or move files yourself |
| `too_complex` / `memory_limit` counts | informational: those attachments are skipped; report the counts |
| `hermes config set` fails | report; never edit `config.yaml` by hand |
| scheduled task registration refused | offer the Hermes cron job or no refresh |
