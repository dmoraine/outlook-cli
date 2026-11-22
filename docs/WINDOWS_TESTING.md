# Testing outlook-index on Windows

Owner procedure for the branch `feat/mail-index`, from a fresh pull to a searchable index. Commands are for WSL
(bash); PowerShell equivalents are at the end. Everything runs locally; the only network access is
`model fetch` (step 4).

**Privacy rule for feedback**: paste back only what section 10 lists. Those outputs are aggregates (counts, sizes,
durations, error kinds). Never paste search hits, excerpts, subjects, names, addresses, folder or file names.

## 1. Pull and build

```bash
cd ~/outlook-cli                  # your clone
git fetch origin
git switch feat/mail-index
git pull --ff-only origin feat/mail-index
git rev-parse --short HEAD        # paste back
scripts/setup-toolchain.sh        # first time only
scripts/build.sh                  # tests + dist/outlook-cli.exe, dist/outlook-index.exe
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

## 4. Model files

The default model of a new index is `e5-base` (~1.1 GB). `e5-small` (~490 MB) is optional.

```bash
outlook-index model fetch          # e5-base; resumable; checks SHA-256
outlook-index model list --pretty  # present: true for each file
outlook-index bench-embed --pretty # ~30 s, synthetic text; paste back
```

`bench-embed` prints `passages_per_s` for the default `--workers`/`--threads` (14 logical CPUs: 4 workers × 3
threads). Each worker is a process with its own copy of the model: if RAM is tight, lower `--workers` and use the
same value for `sync`.

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
- One sync or embed at a time per index: a second one fails at once (`usage`, "another sync is running").
- A sync that would delete more than max(100, 20 %) of the indexed messages aborts with no change. Report it
  before rerunning with `--allow-mass-delete`.
- Deleted Items and Junk E-mail are excluded by default (`--include-folders deleted,junk` to include them).

## 6. Backup

After a complete first sync, with no run in progress (`status`: `"running": null`):

```bash
idx="$(wslpath "$(cmd.exe /c 'echo %LOCALAPPDATA%' 2>/dev/null | tr -d '\r')")/outlook-cli"
mkdir -p "$idx/backup"
cp "$idx/index.sqlite" "$idx/backup/"
[ -f "$idx/index.sqlite-wal" ] && cp "$idx/index.sqlite-wal" "$idx/backup/"
```

The index can always be rebuilt from the OST; the backup saves the hours of embedding.

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
(keep `backup\` and `models\`). To restore, copy the backup files back to `%LOCALAPPDATA%\outlook-cli\`.

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
| 1–2 | `git rev-parse --short HEAD`, `outlook-index --version` |
| 3 | the whole JSON of `doctor` and of `doctor --attachments`, and any stderr line |
| 4 | the whole JSON of `bench-embed` (both models if you fetched both), and any `model fetch` error |
| 5 | the summary JSON of each `sync` (stdout), its wall-clock time, the `--workers`/`--max-time` used, the last `{"progress":...}` line, and any `{"warning":...}` or `{"error":...}` line |
| 5 | `status --pretty` and `scope --pretty` after the run |
| 7 | per query: mode, `count`, time, and the kind of any warning; describe the query generically ("two French words"), not its text |

Error lines are `{"error":{"kind":...,"message":...}}`: paste the `kind`, and the message only after replacing any
path, folder or file name in it with `<redacted>`. Never paste search output, excerpts, subjects, names,
addresses, folder names, attachment names or body text.
