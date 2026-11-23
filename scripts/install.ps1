<#
.SYNOPSIS
Installs outlook-cli.exe and outlook-index.exe for the current user (no admin). Windows PowerShell 5.1 and 7.

.DESCRIPTION
Install dir: %LOCALAPPDATA%\Programs\outlook-cli\, added to the user PATH if missing. Idempotent.
  -Version vX.Y.Z|latest  GitHub Release assets of -Repo, checked against SHA256SUMS (fail closed); when `gh` is
                          logged in, `gh attestation verify` must pass too (attestation: verified|unavailable|failed).
  -FromDir <dir>          locally built exes instead (scripts/build.ps1).
  -Skill hermes|codex|claude|none  also installs the usage skill (.agents\skills\outlook-cli of this checkout):
                          hermes: copy in <install dir>\skills, added to Hermes skills.external_dirs (`hermes config set`);
                          codex: copy in $CODEX_HOME\skills (default %USERPROFILE%\.codex\skills);
                          claude: copy in %USERPROFILE%\.claude\skills. Copies carry a marker file; a skill directory
                          without it is never replaced or removed. Default none.
  -Uninstall              removes the exes, the PATH entry, the scheduled task and every managed skill (and its Hermes
                          entry); keeps the index and models in %LOCALAPPDATA%\outlook-cli\. -PurgeData -Yes also deletes
                          them (irreversible; -Yes is the explicit agreement, -PurgeData alone is refused).
-WhatIf shows the changes without making them. -Json prints one JSON object on stdout (no mailbox content).

.EXAMPLE
powershell -ExecutionPolicy Bypass -File scripts\install.ps1 -Version latest -Skill hermes -Json
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
    # Agreement to -PurgeData; a plain switch, so it binds under `powershell -File` (5.1 and 7), unlike -Confirm:$false.
    [Parameter(ParameterSetName = 'Uninstall')][switch]$Yes,
    [Parameter(ParameterSetName = 'Release')][Parameter(ParameterSetName = 'FromDir')]
    [ValidateSet('hermes', 'codex', 'claude', 'none')][string]$Skill = 'none',
    [switch]$Json
)
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue' # Invoke-WebRequest is very slow with a progress bar in 5.1
$Cmdlet = $PSCmdlet
$Exes = @('outlook-cli.exe', 'outlook-index.exe')
$TaskName = 'outlook-index sync'
$SkillSrc = Join-Path (Split-Path $PSScriptRoot) '.agents\skills\outlook-cli'
$Marker = '.outlook-cli-managed'
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
# Skill parent directories per agent ($HOME is not used: 7 reads the profile folder from the OS, not USERPROFILE).
function Skill-Dirs([string]$dest) {
    $userHome = if ($env:USERPROFILE) { $env:USERPROFILE } else { $HOME }
    $codexHome = if ($env:CODEX_HOME) { $env:CODEX_HOME } else { Join-Path $userHome '.codex' }
    [ordered]@{ hermes = (Join-Path $dest 'skills'); codex = (Join-Path $codexHome 'skills')
        claude = (Join-Path $userHome '.claude\skills') }
}
# (Re)places <dir>\outlook-cli, only if absent or made by this script.
function Copy-Skill([string]$dir) {
    $d = Join-Path $dir 'outlook-cli'
    if ((Test-Path -LiteralPath $d) -and -not (Test-Path -LiteralPath (Join-Path $d $Marker))) {
        throw "$d exists and was not installed by this script: left alone"
    }
    if (-not (Want $d 'Install usage skill')) { return }
    if (Test-Path -LiteralPath $d) { Remove-Item -LiteralPath $d -Recurse -Force }
    New-Item -ItemType Directory -Force -Path $dir | Out-Null
    Copy-Item -LiteralPath $SkillSrc -Destination $d -Recurse
    New-Item -ItemType File -Path (Join-Path $d $Marker) | Out-Null
}
function Remove-Skill([string]$dir) {
    $d = Join-Path $dir 'outlook-cli'
    if ((Test-Path -LiteralPath (Join-Path $d $Marker)) -and (Want $d 'Remove usage skill')) {
        Remove-Item -LiteralPath $d -Recurse -Force
        return $true
    }
    $false
}
# Hermes skills.external_dirs: read as JSON (`config get --json`), written back as a YAML flow list of single-quoted
# strings (`config set` parses YAML): no double quotes, which 5.1 drops from native arguments.
function Get-HermesDirs {
    $ErrorActionPreference = 'Continue'
    $t = ((& hermes config get skills.external_dirs --json 2> $null) | Out-String).Trim()
    if ($LASTEXITCODE -ne 0) { throw 'hermes config get skills.external_dirs failed' }
    if (-not $t -or $t -eq 'null') { return , @() }
    $v = $t | ConvertFrom-Json # 5.1 returns the array as one object: enumerate it with foreach
    $list = @()
    foreach ($x in $v) { $list += [string]$x }
    , $list
}
function Set-HermesDirs([string[]]$dirs) {
    $yaml = '[' + (@($dirs | ForEach-Object { "'" + ($_ -replace "'", "''") + "'" }) -join ', ') + ']'
    $ErrorActionPreference = 'Continue'
    & hermes config set skills.external_dirs $yaml *> $null
    if ($LASTEXITCODE -ne 0) { throw 'hermes config set skills.external_dirs failed' }
}

$tmp = $null
try {
    if (-not $env:LOCALAPPDATA) { throw 'LOCALAPPDATA is not set' }
    $dest = Join-Path $env:LOCALAPPDATA 'Programs\outlook-cli'
    $data = Join-Path $env:LOCALAPPDATA 'outlook-cli'

    if ($Uninstall) {
        if ($PurgeData -and -not $Yes) {
            throw '-PurgeData deletes the index and models: add -Yes once the user agreed'
        }
        $o = [ordered]@{ ok = $true; action = 'uninstall'; install_dir = $dest; removed = $false; path_removed = $false
            task_removed = $false; skills_removed = @(); data_purged = $false }
        Assert-NotRunning $dest
        $sd = Skill-Dirs $dest
        if (Get-Command hermes -ErrorAction SilentlyContinue) {
            $dirs = Get-HermesDirs
            $keep = @($dirs | Where-Object { -not (Same-Dir $_ $sd.hermes) })
            if ($keep.Count -ne $dirs.Count -and (Want 'Hermes skills.external_dirs' "Remove $($sd.hermes)")) {
                Set-HermesDirs $keep
                $o.skills_removed += 'hermes'
            }
        }
        foreach ($a in 'codex', 'claude') { if (Remove-Skill $sd[$a]) { $o.skills_removed += $a } }
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
    if ($Skill -ne 'none') {
        if (-not (Test-Path -LiteralPath (Join-Path $SkillSrc 'SKILL.md'))) { throw "-Skill: .agents\skills\outlook-cli not found (run from a checkout)" }
        if ($Skill -eq 'hermes' -and -not (Get-Command hermes -ErrorAction SilentlyContinue)) {
            throw '-Skill hermes: hermes not found on PATH (Hermes in WSL: scripts/install.sh --skill hermes)'
        }
    }
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
    $o.skill = $Skill
    if ($Skill -ne 'none') {
        $sd = (Skill-Dirs $dest)[$Skill]
        Copy-Skill $sd
        $o.skill_dir = Join-Path $sd 'outlook-cli'
        if ($Skill -eq 'hermes') {
            $dirs = Get-HermesDirs
            $o.hermes_registered = $false
            if (-not @($dirs | Where-Object { Same-Dir $_ $sd }).Count -and (Want 'Hermes skills.external_dirs' "Add $sd")) {
                Set-HermesDirs (@($dirs) + $sd)
                $o.hermes_registered = $true
            }
        }
    }
    Finish $o
} catch {
    $msg = $_.Exception.Message
    if ($Json) { [ordered]@{ ok = $false; error = $msg } | ConvertTo-Json -Compress } else { Write-Error $msg -ErrorAction Continue }
    exit 1
} finally {
    if ($tmp) { Remove-Item -LiteralPath $tmp -Recurse -Force -ErrorAction SilentlyContinue }
}
