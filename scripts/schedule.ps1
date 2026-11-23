<#
.SYNOPSIS
Per-user scheduled task "outlook-index sync" (no admin). Windows PowerShell 5.1 and 7.

.DESCRIPTION
  -Every 1h|4h|daily [-At HH:mm]  registers (or updates) the task: runs `outlook-index.exe sync --progress none`
                                  from %LOCALAPPDATA%\Programs\outlook-cli\ (scripts\install.ps1), only on AC power
                                  (stops on battery), never two at once (the task does not overlap and the sync lock
                                  makes a second sync exit), below-normal priority, only while the user is logged on.
                                  This script is copied next to the exe and run by the task with -Run.
  -Status                         task state, last run time and exit code, last sync summary (aggregates only).
  -Remove                         removes the task (logs kept).
  -Run -Exe <exe> [-LogDir <dir>] what the task runs: one sync, logged to
                                  %LOCALAPPDATA%\outlook-cli\logs\sync-<UTC time>.json (the 14 newest kept).
-WhatIf shows the changes without making them. -Json prints one JSON object on stdout (no mailbox content).

.EXAMPLE
powershell -ExecutionPolicy Bypass -File scripts\schedule.ps1 -Every 4h -At 09:00 -Json
#>
[CmdletBinding(SupportsShouldProcess = $true, DefaultParameterSetName = 'Status')]
param(
    [Parameter(Mandatory = $true, ParameterSetName = 'Register')][ValidateSet('1h', '4h', 'daily')][string]$Every,
    [Parameter(ParameterSetName = 'Register')][ValidatePattern('^([01]\d|2[0-3]):[0-5]\d$')][string]$At = '09:00',
    [Parameter(ParameterSetName = 'Status')][switch]$Status,
    [Parameter(Mandatory = $true, ParameterSetName = 'Remove')][switch]$Remove,
    [Parameter(Mandatory = $true, ParameterSetName = 'Run')][switch]$Run,
    [Parameter(Mandatory = $true, ParameterSetName = 'Run')][string]$Exe,
    [Parameter(ParameterSetName = 'Run')][string]$LogDir,
    [switch]$Json
)
$ErrorActionPreference = 'Stop'
$Cmdlet = $PSCmdlet
$TaskName = 'outlook-index sync'
$Keep = 14
$script:planned = @()

function Want([string]$target, [string]$action) {
    if ($WhatIfPreference) { $script:planned += "$action $target"; return $false }
    $Cmdlet.ShouldProcess($target, $action)
}
function Finish($o) {
    if ($WhatIfPreference) { $o['what_if'] = $true; $o['planned'] = $script:planned }
    if ($Json) { $o | ConvertTo-Json -Depth 10 -Compress }
    else {
        $script:planned | ForEach-Object { Write-Host "What if: $_" }
        Write-Host (([pscustomobject]$o | Format-List | Out-String).Trim())
    }
}
function Utc($t) { if ($t -and $t.Year -ge 2000) { $t.ToUniversalTime().ToString('yyyy-MM-ddTHH:mm:ssZ') } }
function Parse([string]$text) {
    if (-not $text -or -not $text.Trim()) { return $null }
    try { $text | ConvertFrom-Json } catch { $null }
}
function Get-Task { Get-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue }

try {
    if (-not $env:LOCALAPPDATA) { throw 'LOCALAPPDATA is not set' }
    $dest = Join-Path $env:LOCALAPPDATA 'Programs\outlook-cli'
    $logs = Join-Path $env:LOCALAPPDATA 'outlook-cli\logs'

    switch ($PSCmdlet.ParameterSetName) {
        'Run' {
            # Children (sync, embedding workers, extractor) inherit the below-normal class.
            [Diagnostics.Process]::GetCurrentProcess().PriorityClass = 'BelowNormal'
            if (-not $LogDir) { $LogDir = $logs }
            New-Item -ItemType Directory -Force -Path $LogDir | Out-Null
            $out = Join-Path $LogDir '.sync.out'
            $err = Join-Path $LogDir '.sync.err'
            $started = Get-Date
            try {
                $p = Start-Process -FilePath $Exe -ArgumentList 'sync', '--progress', 'none' -NoNewWindow -Wait -PassThru `
                    -RedirectStandardOutput $out -RedirectStandardError $err
                $code = $p.ExitCode
                $summary = Parse ([IO.File]::ReadAllText($out))
                # stderr is JSON from outlook-index (sanitized); anything else is dropped.
                $error1 = $null
                foreach ($l in [IO.File]::ReadAllLines($err)) { $j = Parse $l; if ($null -ne $j) { $error1 = $j } }
            } catch {
                $code = -1; $summary = $null; $error1 = [ordered]@{ error = 'cannot start outlook-index.exe' }
            }
            Remove-Item -LiteralPath $out, $err -Force -ErrorAction SilentlyContinue
            $rec = [ordered]@{ started_at = (Utc $started); finished_at = (Utc (Get-Date)); exit_code = $code
                summary = $summary; error = $error1 }
            $name = 'sync-' + $started.ToUniversalTime().ToString('yyyyMMdd-HHmmss') + '.json'
            [IO.File]::WriteAllText((Join-Path $LogDir $name), ($rec | ConvertTo-Json -Depth 10 -Compress))
            Get-ChildItem -LiteralPath $LogDir -Filter 'sync-*.json' | Sort-Object Name -Descending |
                Select-Object -Skip $Keep | Remove-Item -Force
            if ($Json) { $rec | ConvertTo-Json -Depth 10 -Compress }
            exit $code
        }
        'Register' {
            $exe = Join-Path $dest 'outlook-index.exe'
            if (-not (Test-Path -LiteralPath $exe)) { throw 'outlook-index.exe is not installed; run scripts\install.ps1 first' }
            $self = Join-Path $dest 'schedule.ps1'
            $ps = Join-Path $env:SystemRoot 'System32\WindowsPowerShell\v1.0\powershell.exe'
            $argv = "-NoProfile -NonInteractive -ExecutionPolicy Bypass -WindowStyle Hidden -File `"$self`" -Run -Exe `"$exe`" -LogDir `"$logs`""
            $time = [datetime]::ParseExact($At, 'HH:mm', [Globalization.CultureInfo]::InvariantCulture)
            $trigger = if ($Every -eq 'daily') { New-ScheduledTaskTrigger -Daily -At $time }
            else { New-ScheduledTaskTrigger -Once -At $time -RepetitionInterval (New-TimeSpan -Hours ([int]$Every.TrimEnd('h'))) }
            # Defaults kept on purpose: no start on battery, stop when going on battery.
            $settings = New-ScheduledTaskSettingsSet -MultipleInstances IgnoreNew -Priority 7 -StartWhenAvailable
            $principal = New-ScheduledTaskPrincipal -UserId ([Security.Principal.WindowsIdentity]::GetCurrent().Name) `
                -LogonType Interactive -RunLevel Limited
            $o = [ordered]@{ ok = $true; action = 'register'; task = $TaskName; every = $Every; at = $At; log_dir = $logs }
            $existed = [bool](Get-Task)
            if (Want $TaskName "Register scheduled task (every $Every from $At)") {
                if ($PSCommandPath -ine $self) { Copy-Item -LiteralPath $PSCommandPath -Destination $self -Force }
                Register-ScheduledTask -TaskName $TaskName -Description 'outlook-index sync (outlook-cli scripts\schedule.ps1)' `
                    -Action (New-ScheduledTaskAction -Execute $ps -Argument $argv) -Trigger $trigger -Settings $settings `
                    -Principal $principal -Force | Out-Null
                $o.changed = if ($existed) { 'updated' } else { 'created' }
            }
            Finish $o
        }
        'Remove' {
            $o = [ordered]@{ ok = $true; action = 'remove'; task = $TaskName; removed = $false }
            if ((Get-Task) -and (Want $TaskName 'Remove scheduled task')) {
                Unregister-ScheduledTask -TaskName $TaskName -Confirm:$false
                $o.removed = $true
            }
            $self = Join-Path $dest 'schedule.ps1'
            if ((Test-Path -LiteralPath $self) -and (Want $self 'Remove file')) { Remove-Item -LiteralPath $self -Force }
            Finish $o
        }
        'Status' {
            $t = Get-Task
            $task = [ordered]@{ name = $TaskName; registered = [bool]$t }
            if ($t) {
                $i = $t | Get-ScheduledTaskInfo
                $task.state = "$($t.State)"
                $task.last_run_time = Utc $i.LastRunTime
                $task.last_task_result = $i.LastTaskResult
                $task.next_run_time = Utc $i.NextRunTime
            }
            $files = @(Get-ChildItem -LiteralPath $logs -Filter 'sync-*.json' -ErrorAction SilentlyContinue | Sort-Object Name)
            $last = if ($files.Count) { Parse (Get-Content -LiteralPath $files[-1].FullName -Raw) }
            Finish ([ordered]@{ ok = $true; action = 'status'; task = $task; last_run = $last; logs = $files.Count })
        }
    }
} catch {
    $msg = $_.Exception.Message
    if ($Json) { [ordered]@{ ok = $false; error = $msg } | ConvertTo-Json -Compress } else { Write-Error $msg -ErrorAction Continue }
    exit 1
}
