# Capability inventory (setup step 0)

Read-only probes: they install, enable, log in to or change nothing, and need no consent. Run them, keep the
answers as yes/no, versions and counts, then pick the path from the decision table. Report aggregates only: no
user name, host or proxy name, distro name, path or file name. A probe that fails or is missing a command just
means "absent". Nothing listed here is a prerequisite: Windows PowerShell 5.1 (on every Windows 10/11) and the
built-in `curl.exe` cover the default path; every other tool only opens an optional path.

## Probes

Native PowerShell (5.1 or 7). From WSL, run the block through `powershell.exe -NoProfile -Command '…'`.

```powershell
$PSVersionTable.PSVersion; $env:OS                                # 5.1 = Windows PowerShell, 7.x = pwsh
Get-Command pwsh, powershell -ErrorAction SilentlyContinue | Select-Object Name, Version
Get-ExecutionPolicy -List                                         # read only; never Set-ExecutionPolicy beyond -Scope Process
git --version                                                     # absent: "not recognized"
gh --version; gh auth status                                      # exit 0 = logged in; never print or paste the token
winget --version
curl.exe --version                                                # Windows 10 1803+; "Schannel" = uses the Windows certificate store
Get-Command wsl.exe -ErrorAction SilentlyContinue; wsl.exe -l -q  # count distros only, do not report their names
$cs = Get-CimInstance Win32_ComputerSystem; $os = Get-CimInstance Win32_OperatingSystem
"{0} logical CPUs, {1:N1} GB RAM, {2:N1} GB free" -f $cs.NumberOfLogicalProcessors, ($cs.TotalPhysicalMemory / 1GB), ($os.FreePhysicalMemory / 1MB)
"{0:N1} GB free on the data drive" -f ((Get-PSDrive ($env:LOCALAPPDATA.Substring(0, 1))).Free / 1GB)
```

WSL side: `echo "$WSL_DISTRO_NAME"` (set = inside WSL), `command -v powershell.exe cmd.exe git gh curl`.

Hermes (only if `hermes --version` answers; commands as in Hermes Agent v0.23.3 help and its cron docs; check
`hermes <command> --help` on the user's version before relying on one):

```bash
hermes --version
hermes config path        # its parent is the Hermes home
hermes gateway status     # cron jobs run inside the gateway daemon
hermes cron status        # scheduler alive (gateway process, ticker heartbeat, last tick)
```

Proxy (report set / not set only, never the value):

```powershell
netsh winhttp show proxy                                          # machine WinHTTP proxy ("Direct access" = none)
Get-ItemProperty 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Internet Settings' |
    Select-Object ProxyEnable, @{n = 'ProxyServer'; e = { [bool]$_.ProxyServer } }, @{n = 'AutoConfigURL'; e = { [bool]$_.AutoConfigURL } }
'HTTPS_PROXY', 'HTTP_PROXY', 'NO_PROXY' | ForEach-Object { "$_ set: $([bool][Environment]::GetEnvironmentVariable($_))" }
```

Network reachability: HEAD requests only, nothing downloaded, run when about to pick the install path (say so to
the user). The two checks matter separately: `install.ps1` downloads through .NET (`[Net.WebRequest]`, which uses
the Windows user proxy settings); `outlook-index model fetch` runs `curl.exe`, which uses only the `HTTPS_PROXY`
variable, not those settings.

```powershell
# Release path, in Windows PowerShell 5.1 (`powershell.exe`): status, and the certificate issuer seen for github.com.
$q = [Net.WebRequest]::Create('https://github.com'); $q.Method = 'HEAD'; $q.Timeout = 20000
$r = $q.GetResponse(); [int]$r.StatusCode; $q.ServicePoint.Certificate.Issuer; $r.Close()
# Model path: HTTP status only (000 = unreachable).
curl.exe -sS -I -o NUL -w "%{http_code}`n" --max-time 20 https://huggingface.co
```

An issuer that is not a public certificate authority (the company's own CA name) means TLS inspection: report
"TLS inspection: yes", not the name.

## Decision table

| Capability found | Path |
|---|---|
| `gh` absent | Normal release path, unchanged: `install.ps1 -Version latest -Json` → `attestation: unavailable`, `attestation_reason: gh_missing`. Tell the user: the SHA-256 check against the release's `SHA256SUMS` still runs fail closed over https and nothing is installed on a mismatch; the attestation only adds build provenance on top. |
| `gh` absent, `winget` present | Optionally offer `winget install --id GitHub.cli` (consent; a download; it may ask for elevation) then `gh auth login`, which the user runs. Declined: release path above. |
| `gh` present, `gh auth status` fails | `attestation_reason: gh_not_logged_in`. Optionally offer `gh auth login` (the user runs it, interactive); otherwise release path as for no `gh`. |
| `gh` logged in | `attestation: verified` expected; `failed` = stop, install nothing, offer a source build. |
| no `git` | Not needed to install. For the scripts without a clone: the source archive (GitHub page → Code → Download ZIP, or `https://github.com/dmoraine/outlook-cli/archive/refs/heads/main.zip`, or the release's *Source code (zip)*), extracted to a new folder; run its scripts with the process-scoped bypass below (it also covers the downloaded-file mark). Upgrade = a new archive. |
| PowerShell 7 absent | Nothing to do: Windows PowerShell 5.1 runs every script. |
| WSL absent | Native PowerShell path (default). WSL present: either path; the `.exe` always runs on Windows. |
| Execution policy `Restricted`/`AllSigned` (no `MachinePolicy`/`UserPolicy`) | Process-scoped bypass only: `powershell -NoProfile -ExecutionPolicy Bypass -File …` or `Set-ExecutionPolicy -Scope Process Bypass`. Never change `CurrentUser` or `LocalMachine`. |
| `MachinePolicy` or `UserPolicy` set (group policy) | It overrides the bypass: tell the user, do not work around it. WSL present: `scripts/install.sh --release latest` installs; refresh via Hermes cron or none. |
| Proxy set, release HEAD ok, `curl.exe` fails | For `model fetch` only: the user gives the proxy URL, set for that process (`$env:HTTPS_PROXY = '…'`; from WSL add it to `WSLENV`); or the manual model download + `--model-dir` (README). Never write it to the user or machine environment. |
| TLS inspection | Release: works when the company CA is in the Windows store (the probe answered); SHA-256 still checked. `curl.exe` (Schannel) uses the same store; an error about revocation or the certificate: report it, offer the manual model download. Never disable certificate checks. |
| No network to github.com | Offline: source build from a local checkout or archive, with a toolchain obtained another way, or `-FromDir` exes built elsewhere (`attestation_reason: not_applicable`). |
| Hermes present, `hermes cron status` shows the scheduler running | Hermes cron job may be offered at gate 3 ([hermes-cron.md](hermes-cron.md)). Not running: offer the Windows scheduled task or none; do not start or install the gateway for this. |
| < 8 GB free RAM | Prefer the release over a source build (else `-Jobs 1`); fewer `--workers`; e5-small. |
| Low disk (< model + index estimate, step 1) | e5-small, narrower `--embed-since`, fewer backups; tell the user before gate 1. |
