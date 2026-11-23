<#
.SYNOPSIS
Installs outlook-cli.exe and outlook-index.exe for the current user (no admin). Windows PowerShell 5.1 and 7.

.DESCRIPTION
Install dir: %LOCALAPPDATA%\Programs\outlook-cli\, added to the user PATH if missing. Idempotent.
  -Version vX.Y.Z|latest  GitHub Release assets of -Repo, checked against SHA256SUMS (fail closed); when `gh` is
                          logged in, `gh attestation verify` must pass too (attestation: verified|unavailable|failed).
  -FromDir <dir>          locally built exes instead (scripts/build.ps1).
  -Uninstall              removes the exes, the PATH entry and the scheduled task; keeps the index and models in
                          %LOCALAPPDATA%\outlook-cli\ unless -PurgeData, which requires -Confirm (or -Confirm:$false).
-WhatIf shows the changes without making them. -Json prints one JSON object on stdout (no mailbox content).

.EXAMPLE
powershell -ExecutionPolicy Bypass -File scripts\install.ps1 -Version latest -Json
#>
[CmdletBinding(SupportsShouldProcess = $true, ConfirmImpact = 'Medium', DefaultParameterSetName = 'Release')]
param(
    [Parameter(ParameterSetName = 'Release')][string]$Version = 'latest',
    [Parameter(ParameterSetName = 'Release')][string]$Repo = 'dmoraine/outlook-cli',
    # Release download base: an https URL or a local directory (mirrors, tests).
    [Parameter(ParameterSetName = 'Release')][string]$BaseUrl,
    [Parameter(Mandatory = $true, ParameterSetName = 'FromDir')][string]$FromDir,
    [Parameter(Mandatory = $true, ParameterSetName = 'Uninstall')][switch]$Uninstall,
    [Parameter(ParameterSetName = 'Uninstall')][switch]$PurgeData,
    [switch]$Json
)
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue' # Invoke-WebRequest is very slow with a progress bar in 5.1
$Cmdlet = $PSCmdlet
$Exes = @('outlook-cli.exe', 'outlook-index.exe')
$TaskName = 'outlook-index sync'
$script:planned = @()

# State changes go through Want: under -WhatIf they are listed (not printed, to keep -Json output clean).
function Want([string]$target, [string]$action) {
    if ($WhatIfPreference) { $script:planned += "$action $target"; return $false }
    $Cmdlet.ShouldProcess($target, $action)
}
function Finish($o) {
    if ($WhatIfPreference) { $o['what_if'] = $true; $o['planned'] = $script:planned }
    if ($Json) { $o | ConvertTo-Json -Depth 6 -Compress }
    else {
        $script:planned | ForEach-Object { Write-Host "What if: $_" }
        Write-Host (([pscustomobject]$o | Format-List | Out-String).Trim())
    }
}
function Test-Native([string]$exe, [string[]]$arguments) {
    $ErrorActionPreference = 'Continue' # native stderr must not throw in 5.1
    & $exe @arguments *> $null
    $LASTEXITCODE -eq 0
}
function Same-Dir([string]$a, [string]$b) {
    [Environment]::ExpandEnvironmentVariables($a.Trim()).TrimEnd('\') -ieq $b.TrimEnd('\')
}
# User PATH read and written raw (REG_EXPAND_SZ kept): [Environment]::SetEnvironmentVariable would expand
# %VARS% in the other entries.
function Get-UserPath {
    $k = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey('Environment')
    try { [string]$k.GetValue('Path', '', 'DoNotExpandEnvironmentNames') } finally { $k.Close() }
}
function Set-UserPath([string]$value) {
    $k = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey('Environment', $true)
    try {
        $kind = if ($k.GetValueNames() -contains 'Path') { $k.GetValueKind('Path') } else { 'ExpandString' }
        $k.SetValue('Path', $value, $kind)
    } finally { $k.Close() }
    # ponytail: setting a dummy user variable broadcasts WM_SETTINGCHANGE without P/Invoke, so new shells see PATH.
    [Environment]::SetEnvironmentVariable('OUTLOOK_CLI_PATH_REFRESH', '1', 'User')
    [Environment]::SetEnvironmentVariable('OUTLOOK_CLI_PATH_REFRESH', $null, 'User')
}
function Assert-NotRunning([string]$dir) {
    $p = Get-Process -Name 'outlook-cli', 'outlook-index' -ErrorAction SilentlyContinue |
        Where-Object { $_.Path -and (Same-Dir (Split-Path $_.Path) $dir) }
    if ($p) { throw 'outlook-cli or outlook-index is running from the install dir (a scheduled sync?); stop it first' }
}
# Fetches one release asset: https only, or a copy from a local directory.
function Get-Asset([string]$base, [string]$path, [string]$name, [string]$dest) {
    if ($base -match '^https://') {
        Invoke-WebRequest -UseBasicParsing -Uri "$base/$path/$name" -OutFile $dest
    } elseif ($base -match '^[a-z][a-z0-9+.-]*://') {
        throw "-BaseUrl must be https:// or a local directory"
    } else {
        Copy-Item -LiteralPath (Join-Path $base (Join-Path $path $name)) -Destination $dest
    }
}
# Fail closed: exactly one SHA256SUMS line per exe, and it must match.
function Assert-Sums([string]$dir) {
    $lines = Get-Content -LiteralPath (Join-Path $dir 'SHA256SUMS')
    foreach ($e in $Exes) {
        $m = @($lines | Where-Object { $_ -match ('^([0-9a-fA-F]{64}) [ *]' + [regex]::Escape($e) + '$') })
        if ($m.Count -ne 1) { throw "SHA256SUMS: expected one line for $e" }
        $want = ($m[0] -split ' ')[0]
        $got = (Get-FileHash -LiteralPath (Join-Path $dir $e) -Algorithm SHA256).Hash
        if ($got -ine $want) { throw "SHA-256 mismatch for ${e}: not installed" }
    }
}

$tmp = $null
try {
    if (-not $env:LOCALAPPDATA) { throw 'LOCALAPPDATA is not set' }
    $dest = Join-Path $env:LOCALAPPDATA 'Programs\outlook-cli'
    $data = Join-Path $env:LOCALAPPDATA 'outlook-cli'

    if ($Uninstall) {
        if ($PurgeData -and -not $PSBoundParameters.ContainsKey('Confirm')) {
            throw '-PurgeData deletes the index and models: add -Confirm (or -Confirm:$false once the user agreed)'
        }
        $o = [ordered]@{ ok = $true; action = 'uninstall'; install_dir = $dest; removed = $false; path_removed = $false
            task_removed = $false; data_purged = $false }
        Assert-NotRunning $dest
        if ((Get-Command Get-ScheduledTask -ErrorAction SilentlyContinue) -and
            (Get-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue) -and (Want $TaskName 'Remove scheduled task')) {
            Unregister-ScheduledTask -TaskName $TaskName -Confirm:$false
            $o.task_removed = $true
        }
        if ((Test-Path -LiteralPath $dest) -and (Want $dest 'Remove directory')) {
            Remove-Item -LiteralPath $dest -Recurse -Force
            $o.removed = $true
        }
        $old = Get-UserPath
        # Other entries are kept as they are (empty ones too).
        $parts = @($old -split ';')
        $keep = @($parts | Where-Object { -not ($_ -and (Same-Dir $_ $dest)) })
        if ($keep.Count -ne $parts.Count -and (Want 'user PATH' "Remove $dest")) {
            Set-UserPath ($keep -join ';')
            $o.path_removed = $true
        }
        if ($PurgeData -and (Test-Path -LiteralPath $data) -and (Want $data 'Delete index, models, logs, backups')) {
            Remove-Item -LiteralPath $data -Recurse -Force
            $o.data_purged = $true
        }
        Finish $o
        exit 0
    }

    $o = [ordered]@{ ok = $true; action = 'install'; install_dir = $dest }
    if ($FromDir) {
        $src = $FromDir
        foreach ($e in $Exes) {
            if (-not (Test-Path -LiteralPath (Join-Path $src $e) -PathType Leaf)) { throw "-FromDir: $e not found" }
        }
        $o.source = 'dir'
    } else {
        if ($Version -ne 'latest' -and $Version -notmatch '^v\d+\.\d+\.\d+$') { throw '-Version must be vX.Y.Z or latest' }
        if ($Repo -notmatch '^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$') { throw '-Repo must be owner/name' }
        if (-not $BaseUrl) { $BaseUrl = "https://github.com/$Repo/releases" }
        $path = if ($Version -eq 'latest') { 'latest/download' } else { "download/$Version" }
        $o.source = 'release'; $o.version = $Version; $o.repo = $Repo
        if (Want "$Repo $Version" 'Download and verify release assets') {
            # TLS 1.2 is not on by default in some 5.1 setups.
            [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12
            $tmp = Join-Path ([IO.Path]::GetTempPath()) ('outlook-cli-' + [guid]::NewGuid().ToString('N'))
            New-Item -ItemType Directory -Path $tmp | Out-Null
            foreach ($n in @('SHA256SUMS') + $Exes) { Get-Asset $BaseUrl $path $n (Join-Path $tmp $n) }
            Assert-Sums $tmp
            $o.sha256 = 'verified'
            $o.attestation = 'unavailable'
            if ((Get-Command gh -ErrorAction SilentlyContinue) -and (Test-Native 'gh' @('auth', 'status'))) {
                $o.attestation = 'verified'
                foreach ($e in $Exes) {
                    if (-not (Test-Native 'gh' @('attestation', 'verify', (Join-Path $tmp $e), '--repo', $Repo))) { $o.attestation = 'failed' }
                }
            }
            if ($o.attestation -eq 'failed') { throw 'attestation failed: not installed' }
        }
        $src = $tmp
    }

    if (Want $dest "Install $($Exes -join ', ')") {
        Assert-NotRunning $dest
        New-Item -ItemType Directory -Force -Path $dest | Out-Null
        foreach ($e in $Exes) { Copy-Item -LiteralPath (Join-Path $src $e) -Destination (Join-Path $dest $e) -Force }
    }

    $old = Get-UserPath
    $o.path_added = $false
    if (-not @($old -split ';' | Where-Object { $_ -and (Same-Dir $_ $dest) }).Count -and (Want 'user PATH' "Add $dest")) {
        Set-UserPath $(if (-not $old) { $dest } elseif ($old.EndsWith(';')) { "$old$dest" } else { "$old;$dest" })
        $o.path_added = $true
    }
    if (-not $WhatIfPreference -and -not (($env:Path -split ';') | Where-Object { $_ -and (Same-Dir $_ $dest) })) {
        $env:Path = "$env:Path;$dest" # this session too
    }

    $v = [ordered]@{}
    foreach ($e in $Exes) {
        $f = Join-Path $dest $e
        if (Test-Path -LiteralPath $f) { $v[$e -replace '\.exe$'] = ((& $f --version) | Out-String).Trim() }
    }
    $o.versions = $v
    Finish $o
} catch {
    $msg = $_.Exception.Message
    if ($Json) { [ordered]@{ ok = $false; error = $msg } | ConvertTo-Json -Compress } else { Write-Error $msg -ErrorAction Continue }
    exit 1
} finally {
    if ($tmp) { Remove-Item -LiteralPath $tmp -Recurse -Force -ErrorAction SilentlyContinue }
}
