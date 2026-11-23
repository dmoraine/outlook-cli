# Windows CI: plain assertions for scripts/install.ps1 and scripts/schedule.ps1, under the runner user.
# Run with both Windows PowerShell 5.1 and PowerShell 7: tests/scripts/windows.ps1 -ExeDir target\release
# LOCALAPPDATA points to a temp dir; the user PATH and the scheduled task are restored at the end.
param([Parameter(Mandatory = $true)][string]$ExeDir)
$ErrorActionPreference = 'Stop'
$root = Split-Path (Split-Path $PSScriptRoot)
$install = Join-Path $root 'scripts\install.ps1'
$schedule = Join-Path $root 'scripts\schedule.ps1'
$ExeDir = (Resolve-Path $ExeDir).Path
$task = 'outlook-index sync'
$script:fails = 0
function Check($ok, [string]$what) {
    if ($ok) { Write-Host "ok   $what" } else { Write-Host "FAIL $what"; $script:fails++ }
}
function UserPath {
    $k = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey('Environment')
    try { [string]$k.GetValue('Path', '', 'DoNotExpandEnvironmentNames') } finally { $k.Close() }
}
# Runs a script in a child process of the same edition, as an agent does; stdout must be JSON only.
function Run([string]$file, [string[]]$a) {
    $shell = (Get-Process -Id $PID).Path
    $out = & $shell -NoProfile -NonInteractive -ExecutionPolicy Bypass -File $file @a
    $script:code = $LASTEXITCODE
    $text = ($out | Out-String).Trim()
    try { return ($text | ConvertFrom-Json) } catch { Write-Host "not JSON: $text"; return $null }
}
function Hash([string]$f) { (Get-FileHash $f -Algorithm SHA256).Hash.ToLowerInvariant() }

$before = UserPath
$tmp = Join-Path ([IO.Path]::GetTempPath()) ("outlook-cli-test-" + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $tmp | Out-Null
$env:LOCALAPPDATA = $tmp
$env:GH_TOKEN = $null; $env:GITHUB_TOKEN = $null # gh not logged in: attestation "unavailable"
$dir = Join-Path $tmp 'Programs\outlook-cli'
$data = Join-Path $tmp 'outlook-cli'
function NonEmpty([string]$p) { @($p -split ';' | Where-Object { $_ }) -join ';' }
function Entries { @((UserPath) -split ';' | Where-Object { $_.TrimEnd('\') -ieq $dir }).Count }

try {
    # install -FromDir, twice (idempotent), PATH added once.
    $r = Run $install @('-FromDir', $ExeDir, '-Json')
    Check ($code -eq 0 -and $r.ok -and $r.source -eq 'dir') "install -FromDir ok"
    Check ((Test-Path "$dir\outlook-cli.exe") -and (Test-Path "$dir\outlook-index.exe")) "exes installed"
    Check ($r.path_added -eq $true -and (Entries) -eq 1) "user PATH entry added"
    Check ("$($r.versions.'outlook-index')" -like 'outlook-index *') "outlook-index version printed"
    $r = Run $install @('-FromDir', $ExeDir, '-Json')
    Check ($code -eq 0 -and $r.path_added -eq $false -and (Entries) -eq 1) "second install: PATH unchanged"
    $r = Run $install @('-FromDir', (Join-Path $tmp 'missing'), '-Json')
    Check ($code -ne 0 -and $r.ok -eq $false) "missing -FromDir fails"

    # Uninstall: exes and PATH entry removed, user PATH as before, data kept; idempotent.
    New-Item -ItemType Directory -Force -Path $data | Out-Null
    Set-Content -Path "$data\index.sqlite" -Value 'x'
    $r = Run $install @('-Uninstall', '-Json')
    Check ($code -eq 0 -and $r.ok -and -not (Test-Path $dir)) "uninstall removes the install dir"
    Check ((NonEmpty (UserPath)) -eq (NonEmpty $before)) "user PATH restored"
    Check (Test-Path "$data\index.sqlite") "uninstall keeps the data"
    $r = Run $install @('-Uninstall', '-Json')
    Check ($code -eq 0 -and $r.ok -and $r.removed -eq $false) "second uninstall is a no-op"

    # -WhatIf changes nothing.
    $r = Run $install @('-FromDir', $ExeDir, '-WhatIf', '-Json')
    Check ($code -eq 0 -and $r.what_if -eq $true -and -not (Test-Path $dir) -and (Entries) -eq 0) "install -WhatIf changes nothing"

    # Release path from a local mirror: verified, then fail closed on bad SHA256SUMS.
    $rel = Join-Path $tmp 'rel\download\v0.0.1'
    New-Item -ItemType Directory -Force -Path $rel | Out-Null
    Copy-Item "$ExeDir\outlook-cli.exe", "$ExeDir\outlook-index.exe" $rel
    $sums = "$(Hash "$rel\outlook-cli.exe")  outlook-cli.exe`n$(Hash "$rel\outlook-index.exe")  outlook-index.exe`n"
    [IO.File]::WriteAllText("$rel\SHA256SUMS", $sums)
    $base = Join-Path $tmp 'rel'
    $r = Run $install @('-Version', 'v0.0.1', '-BaseUrl', $base, '-Json')
    Check ($code -eq 0 -and $r.sha256 -eq 'verified' -and $r.attestation -eq 'unavailable') "release install verified"
    Run $install @('-Uninstall', '-Json') | Out-Null
    [IO.File]::WriteAllText("$rel\SHA256SUMS", ($sums -replace '^[0-9a-f]{64}', ('0' * 64)))
    $r = Run $install @('-Version', 'v0.0.1', '-BaseUrl', $base, '-Json')
    Check ($code -ne 0 -and $r.ok -eq $false -and -not (Test-Path "$dir\outlook-cli.exe")) "hash mismatch fails closed"
    [IO.File]::WriteAllText("$rel\SHA256SUMS", (($sums -split "`n")[0] + "`n"))
    $r = Run $install @('-Version', 'v0.0.1', '-BaseUrl', $base, '-Json')
    Check ($code -ne 0 -and -not (Test-Path "$dir\outlook-index.exe")) "missing SHA256SUMS entry fails closed"
    $r = Run $install @('-Version', 'v9.9.9', '-BaseUrl', $base, '-Json')
    Check ($code -ne 0 -and -not (Test-Path "$dir\outlook-cli.exe")) "missing release fails"
    $r = Run $install @('-Version', '1.2', '-Json')
    Check ($code -ne 0) "invalid -Version rejected"

    # -PurgeData needs -Confirm.
    $r = Run $install @('-Uninstall', '-PurgeData', '-Json')
    Check ($code -ne 0 -and (Test-Path "$data\index.sqlite")) "-PurgeData without -Confirm refused"
    $r = Run $install @('-Uninstall', '-PurgeData', '-Confirm:$false', '-Json')
    Check ($code -eq 0 -and $r.data_purged -eq $true -and -not (Test-Path $data)) "-PurgeData -Confirm removes the data"

    # Scheduled task: -WhatIf, register, update, status, run log + rotation, remove.
    Run $install @('-FromDir', $ExeDir, '-Json') | Out-Null
    $r = Run $schedule @('-Every', '4h', '-At', '09:00', '-WhatIf', '-Json')
    Check ($code -eq 0 -and $r.what_if -eq $true -and -not (Get-ScheduledTask -TaskName $task -ErrorAction SilentlyContinue)) "schedule -WhatIf registers nothing"
    $r = Run $schedule @('-Every', '1h', '-At', '09:30', '-Json')
    $t = Get-ScheduledTask -TaskName $task -ErrorAction SilentlyContinue
    Check ($code -eq 0 -and $r.changed -eq 'created' -and $t) "task registered"
    if ($t) {
        Check ($t.Settings.DisallowStartIfOnBatteries -and $t.Settings.StopIfGoingOnBatteries) "AC power only"
        Check ("$($t.Settings.MultipleInstances)" -eq 'IgnoreNew' -and $t.Settings.Priority -eq 7) "no overlap, low priority"
        Check ("$($t.Principal.LogonType)" -eq 'Interactive' -and "$($t.Principal.RunLevel)" -eq 'Limited') "per-user, no elevation"
        Check ($t.Actions[0].Arguments -like '*-Run*' -and $t.Actions[0].Arguments -like '*outlook-index.exe*') "action runs the sync wrapper"
        Check ($t.Triggers[0].Repetition.Interval -eq 'PT1H') "hourly trigger"
    }
    $r = Run $schedule @('-Every', 'daily', '-At', '10:00', '-Json')
    $t = Get-ScheduledTask -TaskName $task -ErrorAction SilentlyContinue
    Check ($code -eq 0 -and $r.changed -eq 'updated' -and $t.Triggers.Count -eq 1 -and $t.Triggers[0].CimClass.CimClassName -eq 'MSFT_TaskDailyTrigger') "task updated to daily"
    $r = Run $schedule @('-Every', '2h', '-Json')
    Check ($code -ne 0) "invalid -Every rejected"

    $logs = Join-Path $data 'logs'
    New-Item -ItemType Directory -Force -Path $logs | Out-Null
    1..20 | ForEach-Object { Set-Content -Path (Join-Path $logs ('sync-20000101-0000{0:d2}.json' -f $_)) -Value '{}' }
    Run $schedule @('-Run', '-Exe', "$dir\outlook-index.exe", '-LogDir', $logs) | Out-Null
    $files = @(Get-ChildItem $logs -Filter 'sync-*.json' | Sort-Object Name)
    Check ($files.Count -eq 14) "log rotation keeps 14 (got $($files.Count))"
    $last = Get-Content $files[-1].FullName -Raw | ConvertFrom-Json
    $keys = @($last.PSObject.Properties.Name | Sort-Object)
    Check (($keys -join ',') -eq 'error,exit_code,finished_at,started_at,summary') "log keys: $($keys -join ',')"
    Check ($last.exit_code -ne 0 -and $code -eq $last.exit_code) "sync exit code logged and returned (no .ost here)"
    $r = Run $schedule @('-Status', '-Json')
    Check ($code -eq 0 -and $r.task.registered -and $r.last_run.exit_code -eq $last.exit_code -and $r.logs -eq 14) "status reports the task and the last run"

    $r = Run $schedule @('-Remove', '-Json')
    Check ($code -eq 0 -and $r.removed -and -not (Get-ScheduledTask -TaskName $task -ErrorAction SilentlyContinue)) "task removed"
    $r = Run $schedule @('-Remove', '-Json')
    Check ($code -eq 0 -and $r.removed -eq $false) "second remove is a no-op"
    $r = Run $schedule @('-Status', '-Json')
    Check ($code -eq 0 -and $r.task.registered -eq $false) "status without a task"

    # build.ps1 -WhatIf: plan only, nothing downloaded or built.
    $r = Run (Join-Path $root 'scripts\build.ps1') @('-WhatIf', '-Json')
    Check ($code -eq 0 -and $r.what_if -eq $true -and @($r.planned).Count -ge 2 -and -not (Test-Path "$data\toolchain")) "build.ps1 -WhatIf plans only"

    # Uninstall also removes a registered task.
    Run $schedule @('-Every', '4h', '-Json') | Out-Null
    $r = Run $install @('-Uninstall', '-Json')
    Check ($code -eq 0 -and $r.task_removed -and -not (Get-ScheduledTask -TaskName $task -ErrorAction SilentlyContinue)) "uninstall removes the task"
} finally {
    Unregister-ScheduledTask -TaskName $task -Confirm:$false -ErrorAction SilentlyContinue
    if ((UserPath) -ne $before) { [Environment]::SetEnvironmentVariable('Path', $before, 'User') }
    Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
}
if ($script:fails) { Write-Host "$($script:fails) failure(s)"; exit 1 }
Write-Host "install.ps1 / schedule.ps1 / build.ps1: all checks passed ($($PSVersionTable.PSEdition) $($PSVersionTable.PSVersion))"
exit 0
