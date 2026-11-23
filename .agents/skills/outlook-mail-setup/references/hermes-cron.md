# Hermes cron refresh (script only, no LLM)

Checked against the Hermes docs (`website/docs/user-guide/features/cron.md`, "No-agent mode (script-only jobs)",
and `guides/cron-script-only.md`) and `hermes cron create --help` of Hermes Agent v0.23.3. Recheck
`hermes cron create --help` on the user's Hermes before running anything; never guess flags.

What Hermes does with such a job: it runs the script on schedule without any model; **empty stdout = silent
tick**; stdout is delivered verbatim; a non-zero exit or a timeout delivers an error alert. Scripts must resolve
inside **`<hermes home>/scripts/`** (`hermes config path` → its parent is the Hermes home; defaults
`%LOCALAPPDATA%\hermes` on Windows, `~/.hermes` elsewhere). `.sh`/`.bash` run under bash, anything else under
Hermes' Python. A script is limited to 3600 s by default (`cron.script_timeout_seconds`), hence `--max-time 50m`
below: a longer refresh resumes at the next tick. The scheduler must be running (`hermes cron status`).

Only after consent gate 3, and only if no Windows scheduled task does the same (`scripts\schedule.ps1 -Status`).

## 1. Script

Same file for Hermes on Windows and in WSL (standard library only). Write it as
`<hermes home>/scripts/outlook-index-refresh.py` and replace `EXE` with the absolute path of `outlook-index` on
this machine (`(Get-Command outlook-index).Source` in PowerShell, usually
`%LOCALAPPDATA%\Programs\outlook-cli\outlook-index.exe`; `command -v outlook-index` in WSL, the
`~/.local/bin/outlook-index` link). Do not rely on `PATH` or other variables: cron scripts get a sanitized
environment.

```python
# Hermes cron, no agent: one outlook-index sync. Silent on success; one aggregate-only line on failure
# (outlook-index error lines hold no mail content, paths or names).
import subprocess
import sys

EXE = r"C:\Users\<user>\AppData\Local\Programs\outlook-cli\outlook-index.exe"
flags = getattr(subprocess, "BELOW_NORMAL_PRIORITY_CLASS", 0)  # Windows; 0 elsewhere
p = subprocess.run([EXE, "sync", "--progress", "none", "--max-time", "50m"], stdout=subprocess.DEVNULL,
                   stderr=subprocess.PIPE, text=True, encoding="utf-8", errors="replace", creationflags=flags)
if p.returncode:
    last = (p.stderr.strip().splitlines() or ["no error line"])[-1]
    print(f"outlook-index sync failed (exit {p.returncode}): {last}")
    sys.exit(p.returncode)
```

## 2. Job

```bash
hermes cron create "every 4h" --no-agent --script outlook-index-refresh.py --name outlook-index-refresh \
    --deliver <target>
```

- Schedule: `30m`, `every 2h`, or a cron expression such as `0 9 * * *` (`--timezone` for another zone).
- `--deliver`: where failure alerts go, chosen with the user (`origin`, a platform such as `telegram`, …;
  `local` sends nothing and leaves the result in `hermes cron list` / `hermes cron runs`).
- `--catch-up off` skips runs missed while the PC was off instead of running once on return.
- Test once: `hermes cron run <job_id>` (next scheduler tick), then `hermes cron runs <job_id>`.

## 3. Manage

`hermes cron list` (job ids), `hermes cron pause <job_id>` / `resume <job_id>` (upgrade), `hermes cron remove
<job_id>` and delete the script (uninstall), `hermes cron doctor` (missing script, failed runs).

Error alerts: exit 2 "another sync is running" = a manual sync overlapped (harmless, next tick retries); exit 2
mass deletion refused, exit 3 `upgrade_pending`, exit 4/5: report to the user (outlook-cli skill §6), do not change
the job's flags.
