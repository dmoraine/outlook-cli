# Testing outlook-index on Windows

Procedure for a release (or the git branch `main`), from install to a searchable index. Commands are for WSL
(bash); without WSL, see section 0 for install and scheduling, then run the same `outlook-index` commands from
PowerShell (section 9). Everything runs locally; the only network access is the release or toolchain download and
`model fetch` (step 4). An AI agent follows [AGENTS.md](../AGENTS.md) and the `outlook-mail-setup` skill instead.

**Privacy rule for feedback**: paste back only what section 10 lists. Those outputs are aggregates (counts, sizes,
durations, error kinds). Never paste search hits, excerpts, subjects, names, addresses, folder or file names.

## 0. Native PowerShell path (no WSL)

Windows PowerShell 5.1 or PowerShell 7, no admin. Allow the scripts for this window only with
`Set-ExecutionPolicy -Scope Process Bypass` (or run each with `powershell -NoProfile -ExecutionPolicy Bypass -File …`).
Each script takes `-WhatIf` (plan only) and `-Json` (one JSON object on stdout, no mailbox content).

```powershell
Set-ExecutionPolicy -Scope Process Bypass
# scripts without git: Code → Download ZIP or the release source zip; with a clone: git switch main; git pull --ff-only origin main
scripts\install.ps1 -Version latest -Json        # release, SHA-256 (+ attestation if gh is logged in), fail closed
scripts\build.ps1 -InstallToolchain -NoTests -Json   # or: source build (rustup, gnullvm, llvm-mingw for the user), then install
outlook-index --version                          # new shell: the install dir is on the user PATH
```

`install.ps1` installs both exes into `%LOCALAPPDATA%\Programs\outlook-cli\` and adds that folder to the user
PATH. `-FromDir <dir>` installs exes built elsewhere. `-Skill hermes|codex|claude` also installs the `outlook-cli`
agent skill (Hermes: registered in `skills.external_dirs` with `hermes config set`; Codex: `$CODEX_HOME\skills`,
default `%USERPROFILE%\.codex\skills`; Claude Code: `%USERPROFILE%\.claude\skills`). `build.ps1` needs about
3 GB of disk with `-NoTests` (5 GB with the tests) and about 8 GB of free RAM (`-Jobs 1` when short).

**Testing a CI dry run before a release** (the `release-assets` artifact of a *Run workflow* run, see
[RELEASING.md](RELEASING.md)). `install.ps1 -BaseUrl <folder> -Version latest` reads
`<folder>\latest\download\`, then checks `SHA256SUMS` as for a release.

- Without `gh`, in a browser: signed in to GitHub (artifacts need a login), open the repository → Actions → CI →
  the dry-run run → section **Artifacts** at the bottom of its page → `release-assets` (a zip). Unzip it into a new
  folder `C:\dl\dryrun\latest\download\` (three files: both exes and `SHA256SUMS`), then:

  ```powershell
  scripts\install.ps1 -Version latest -BaseUrl C:\dl\dryrun -Json   # sha256: verified, attestation_reason: gh_missing
  ```

- With `gh`: `gh run list --workflow ci.yml --event workflow_dispatch -L 1` (run id), then
  `gh run download <run-id> -n release-assets -D C:\dl\dryrun\latest\download` and the same `install.ps1` line.
  A dry run has no attestation, so with `gh` logged in that line reports `attestation failed` and installs nothing
  (fail closed): compare `(Get-FileHash <exe> -Algorithm SHA256).Hash` with its `SHA256SUMS` line by hand, then
  `scripts\install.ps1 -FromDir C:\dl\dryrun\latest\download -Json` (`attestation_reason: not_applicable`).

Then follow sections 3 to 7 from PowerShell. Periodic refresh (per-user scheduled task, AC power only, no overlap,
low priority, JSON logs in `%LOCALAPPDATA%\outlook-cli\logs\`, 14 kept):

```powershell
scripts\schedule.ps1 -Every 4h -At 09:00 -Json   # 1h | 4h | daily
scripts\schedule.ps1 -Status -Json               # last run time, exit code, last sync summary (counts)
scripts\schedule.ps1 -Remove -Json
```

Uninstall: `scripts\install.ps1 -Uninstall -Json` removes the exes, the PATH entry, the scheduled task and the
skills it installed (and their Hermes entry); the index and models stay unless `-PurgeData -Yes` is added (irreversible). WSL:
`scripts/install.sh --uninstall` first (links and WSL-side skills).

## 1. Pull and build

```bash
cd ~/outlook-cli                  # your clone
git fetch origin
git switch main
git pull --ff-only origin main
git rev-parse --short HEAD        # paste back
scripts/setup-toolchain.sh        # first time only
scripts/build.sh                  # tests + dist/outlook-cli.exe, dist/outlook-index.exe
```

The build needs about 8 GB of RAM free in WSL (`free -g`); `cargo` jobs default to 2 (`CARGO_BUILD_JOBS`). If
it is killed or WSL stops (out of memory):

```bash
scripts/build.sh --no-tests                    # build only; CI runs the tests (same as SKIP_TESTS=1)
CARGO_BUILD_JOBS=1 scripts/build.sh --no-tests # one job at a time: slower, least RAM
```

Optional: give WSL more memory in `%UserProfile%\.wslconfig`, then run `wsl --shutdown` from PowerShell:

```ini
[wsl2]
memory=12GB
```

## 2. Install

Do not install while an `outlook-index` process is running: Windows refuses to overwrite a running `.exe`.

```bash
scripts/install.sh                # %LOCALAPPDATA%\Programs\outlook-cli\ + ~/.local/bin/outlook-index
outlook-index --version           # paste back
```

## 3. Diagnostics

```bash
outlook-index doctor --pretty                     # 200 random messages
outlook-index doctor --attachments --pretty       # 300 attachments, each parsed in a child process
```

Both print aggregates only and are meant to be pasted back whole (stdout and any stderr line).
`doctor` also prints `machine.available_ram_bytes` and `machine.defaults`: the extractor memory limit
(`extract_memory_mb`) and the default `workers` per model that `sync` would use on this PC.

**Attachment extraction limits.** The child parses every archive and PDF; the parent only checks sizes and magic
bytes. Past a structural cap (ZIP: 10,000 entries, 16 path levels, 256 MB unpacked per part and 512 MB per file,
unpacked/packed ratio 100 past 16 MB; XML: 256 nesting levels, 10,000,000 elements per part; PDF: 200,000 objects,
10,000 pages; nested archives are never opened) a file is skipped as `too_complex`. The child also runs in a Windows
job object limited to a quarter of the physical memory available at start, clamped to 256 MB..2 GB (1 GB if
unknown); `--extract-memory-mb N` (on `sync` and `doctor --attachments`, at least 128) overrides it. A child past it
dies alone: the file is `memory_limit`, retried like `failed` (3 attempts in all), and the sync goes on with a new
child. `doctor --attachments` shows the limit as `limits.memory_mb` / `limits.memory_enforced`; report any
`too_complex` or `memory_limit` count.

## 4. Model files

The default model of a new index is `e5-base` (~1.1 GB). `e5-small` (~490 MB) is optional.

```bash
outlook-index model fetch          # e5-base; resumable; checks SHA-256
outlook-index model list --pretty  # present: true for each file
outlook-index bench-embed --pretty # ~30 s, synthetic text; paste back
```

`bench-embed` prints `passages_per_s` for the default `--workers`/`--threads` (14 logical CPUs: 4 workers × 3
threads, fewer when memory is short). Each worker is a process with its own copy of the model; the default count
fits the memory available at start less 1 GB, at about 1.25 GB per e5-small worker (measured peak 1.20 GB) and
2.75 GB per e5-base worker (estimated from file sizes). If RAM is still tight, lower `--workers` and use the same
value for `sync`.

## 5. First sync

Optional, for an exact time estimate: index first, then count what is left to embed.

```bash
outlook-index sync --no-embed      # folders, messages, attachment text; no model needed
outlook-index scope --pretty       # pending (body chunks) + attachment_chunks.pending
```

**Time estimate**: (`pending` + `attachment_chunks.pending`) ÷ `passages_per_s` from `bench-embed` with the same
`--workers` and `--threads` = seconds. Example: 50,000 chunks at 4 passages/s ≈ 3 h 30 min. Without the first
step, use the doctor estimates (body chunks plus attachment chunks; each attachment content counts at most 30).

Then run the full sync (it indexes what changed, then embeds), and follow it from a second terminal:

```bash
outlook-index sync                         # progress bar on stderr, summary JSON on stdout
outlook-index sync --max-time 2h           # or in slices; the next sync resumes
outlook-index sync --workers 3             # fewer processes, less RAM
```

```bash
outlook-index status --watch               # one {"running": ...} line every 10 s until the run ends
outlook-index status --pretty              # counts, embedded %, size; "running" while a sync runs
```

- Embedding runs at below-normal priority: the laptop stays usable.
- **Ctrl-C** is safe: every batch is committed, so at most the batches in flight are lost. Run `sync` again to
  resume. `status` shows `"stale": true` for a run that was killed; the next sync resumes it.
- One sync or embed at a time per index: a second one fails after waiting 3 s for the lock (`usage`, "another sync
  is running"). `search`, `status` and `scope` without options are read-only and can run during a sync.
- After updating `outlook-index`, `status` may show `"schema": {..., "upgrade_pending": true}` and `search` fail
  with `upgrade_pending`: the next `sync` upgrades the index.
- A sync that would delete more than max(100, 20 %) of the indexed messages aborts with no change. Report it
  before rerunning with `--allow-mass-delete`.
- Deleted Items and Junk E-mail are excluded by default (`--include-folders deleted,junk` to include them).

## 6. Backup

After a complete first sync (safe even while a sync runs):

```bash
outlook-index backup --pretty      # %LOCALAPPDATA%\outlook-cli\backups\index-YYYYMMDD-HHMMSS.sqlite
```

One consistent snapshot (SQLite `VACUUM INTO`) into a new single file (an existing `--to PATH` is refused), then
reopened read-only and checked: report `integrity` (`"ok"`), `schema_version`, `counts` and `bytes`, not the
`destination`. Never copy `index.sqlite` or its `-wal` by hand: a stale WAL can bring back an old state.

The index can always be rebuilt from the OST; the backup saves the hours of embedding. `%LOCALAPPDATA%` is
per-user; an `--index` elsewhere (shared folder, other drive) keeps the ACLs its owner gives it.

## 7. Search

Check the results yourself; do not paste them.

```bash
outlook-index search "facture fournisseur" -n 5                     # hybrid once embedded
outlook-index search "facture fournisseur" --mode keyword            # every word, accent-insensitive
outlook-index search "unpaid supplier invoice" --mode semantic       # by meaning, across languages
outlook-index search "budget" -F Inbox --since 30d --from dupont --has-attachments
outlook-index search "contrat" --since 2025-01-01 --until 2025-07-01 --format jsonl
```

For feedback, keep only the mode, the hit count and the time:

```bash
time outlook-index search "facture fournisseur" | grep -o '"count":[0-9]*,"mode":"[a-z]*"'
```

## 8. Rollback

Nothing is ever written to the OST, and `outlook-cli` is unchanged. To drop the index, stop any run, then delete
`index.sqlite`, `index.sqlite-wal`, `index.sqlite-shm` and `index.sqlite.lock` in `%LOCALAPPDATA%\outlook-cli\`
(keep `backups\` and `models\`). To restore, with every `outlook-index` stopped (no sync, embed, search or
`status --watch`): delete `index.sqlite-wal` and `index.sqlite-shm` if present, then copy the verified backup to
`index.sqlite`:

```powershell
$d = "$env:LOCALAPPDATA\outlook-cli"
Remove-Item "$d\index.sqlite-wal", "$d\index.sqlite-shm" -ErrorAction SilentlyContinue
Copy-Item "$d\backups\index-YYYYMMDD-HHMMSS.sqlite" "$d\index.sqlite"
```

## 9. WSL and PowerShell notes

- Always run the Windows `.exe` (the `~/.local/bin/outlook-index` link); a Linux program cannot read an OST that
  Outlook keeps open.
- Keep the index on the Windows side (the default `%LOCALAPPDATA%\outlook-cli\index.sqlite`): not under `~` in WSL
  (reached through the `\\wsl.localhost` share) and not under OneDrive (a warning is printed).
- A Windows `.exe` started from WSL only sees the variables listed in `WSLENV`:
  `export OUTLOOK_INDEX_DB=/mnt/c/Users/me/idx/index.sqlite WSLENV=$WSLENV:OUTLOOK_INDEX_DB/p` (same for
  `OUTLOOK_CLI_FILE` and `OUTLOOK_INDEX_MODEL_DIR`). Options such as `--index /mnt/c/...` need nothing.
- `model fetch` runs the `curl.exe` shipped with Windows 10+. In Windows PowerShell 5.1, `curl` alone is an alias of
  `Invoke-WebRequest`: for a manual download type `curl.exe`, then pass `--model-dir`.
- PowerShell:

  ```powershell
  Set-Alias outlook-index "$env:LOCALAPPDATA\Programs\outlook-cli\outlook-index.exe"
  outlook-index doctor --pretty
  outlook-index search "facture" --ascii | ConvertFrom-Json   # --ascii keeps accents intact in PowerShell 5.1
  ```

## 10. What to paste back

| Step | Paste |
|---|---|
| 0–2 | `outlook-index --version`; from a clone also `git rev-parse --short HEAD`; for a release install, `sha256` and `attestation_reason` |
| 3 | the whole JSON of `doctor` and of `doctor --attachments`, and any stderr line |
| 4 | the whole JSON of `bench-embed` (both models if you fetched both), and any `model fetch` error |
| 5 | the summary JSON of each `sync` (stdout), its wall-clock time, the `--workers`/`--max-time` used, the last `{"progress":...}` line, and any `{"warning":...}` or `{"error":...}` line |
| 5 | `status --pretty` and `scope --pretty` after the run |
| 7 | per query: mode, `count`, time, and the kind of any warning; describe the query generically ("two French words"), not its text |

Error lines are `{"error":{"kind":...,"message":...}}`; the message is a fixed text per kind (plus counts, flag
names, model names or a code location), with no path, name or query text: paste the line whole. Do not set
`OUTLOOK_INDEX_DEBUG=1` for feedback: it adds a `debug` field with the raw error, which may hold paths, folder or
file names and mail text; use it only to troubleshoot locally and never paste it. Never paste search output,
excerpts, subjects, names, addresses, folder names, attachment names or body text.
