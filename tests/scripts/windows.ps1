# Windows CI: plain assertions for scripts/install.ps1 (with -Skill) and scripts/schedule.ps1, under the runner user.
# Run with both Windows PowerShell 5.1 and PowerShell 7: tests/scripts/windows.ps1 -ExeDir target\release
# LOCALAPPDATA, USERPROFILE and CODEX_HOME point to a temp dir, hermes is a fake; the user PATH, the scheduled task
# and the environment are restored at the end.
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
    $script:raw = $text
    try { return ($text | ConvertFrom-Json) } catch { Write-Host "not JSON: $text"; return $null }
}
function Hash([string]$f) { (Get-FileHash $f -Algorithm SHA256).Hash.ToLowerInvariant() }
function Snapshot([string]$d) {
    @(Get-ChildItem -LiteralPath $d -Force | Sort-Object Name |
        ForEach-Object { "$($_.Name)|$($_.Length)|$($_.LastWriteTimeUtc.Ticks)" }) -join ';'
}

# Download policy (scripts/download.ps1, used by install.ps1 and build.ps1): https on every hop, at most 10
# redirects, checked before each request. A fake fetcher records the requested URLs.
. (Join-Path $root 'scripts\download.ps1')
$redirects = @{ 'https://a.test/x' = 'https://b.test/y'; 'https://b.test/y' = '/z'; 'https://c.test/x' = 'http://b.test/y'
    'https://loop.test/' = '/' }
$fetch = {
    param([uri]$uri, [string]$dest)
    $script:hops += "$uri"
    $to = $script:redirects["$uri"] # script scope: Save-Https's own $redirects counter shadows a bare $redirects
    if ($to) { return @{ status = 302; location = $to } }
    if ("$uri" -eq 'https://partial.test/') { [IO.File]::WriteAllText($dest, 'part'); throw 'connection reset' }
    if ("$uri" -eq 'https://cached.test/') { return @{ status = 304; location = 'https://a.test/x' } }
    [IO.File]::WriteAllText($dest, 'body'); @{ status = 200 }
}
function Try-Save([string]$url, [string]$dest) {
    $script:hops = @()
    try { Save-Https $url $dest $fetch; $true } catch { $false }
}
$dl = Join-Path ([IO.Path]::GetTempPath()) ("outlook-cli-dl-" + [guid]::NewGuid().ToString('N'))
$listener = $null; $server = $null
try {
    Check ((Try-Save 'https://a.test/x' $dl) -and ($script:hops -join ',') -eq 'https://a.test/x,https://b.test/y,https://b.test/z' -and
        [IO.File]::ReadAllText($dl) -eq 'body') "https redirects (absolute, relative) followed"
    Remove-Item -LiteralPath $dl -Force -ErrorAction SilentlyContinue
    Check (-not (Try-Save 'https://c.test/x' $dl) -and ($script:hops -join ',') -eq 'https://c.test/x' -and -not (Test-Path $dl)) "https to http redirect refused before its request"
    Check (-not (Try-Save 'http://a.test/x' $dl) -and $script:hops.Count -eq 0) "http URL refused before any request"
    Check (-not (Try-Save 'https://loop.test/' $dl) -and $script:hops.Count -eq 11) "10 redirects followed, the 11th refused (requests: $($script:hops.Count))"
    Check (-not (Try-Save 'https://cached.test/' $dl) -and $script:hops.Count -eq 1) "3xx other than a redirect fails"
    Check (-not (Try-Save 'https://partial.test/' $dl) -and -not (Test-Path $dl)) "failed download leaves no partial file"
    # The real hop (Invoke-Hop) against a loopback server: a 302 is returned, not followed; a 200 body is written.
    $listener = New-Object Net.Sockets.TcpListener([Net.IPAddress]::Loopback, 0)
    $listener.Start()
    $server = [powershell]::Create().AddScript({
            param($l)
            foreach ($resp in "HTTP/1.1 302 Found`r`nLocation: https://example.invalid/next`r`nContent-Length: 0`r`nConnection: close`r`n`r`n",
                "HTTP/1.1 200 OK`r`nContent-Length: 4`r`nConnection: close`r`n`r`nbody") {
                $c = $l.AcceptTcpClient()
                $s = $c.GetStream()
                $rd = New-Object IO.StreamReader($s)
                while ($rd.ReadLine()) { }
                $b = [Text.Encoding]::ASCII.GetBytes($resp)
                $s.Write($b, 0, $b.Length)
                $c.Close()
            }
        }).AddArgument($listener)
    $null = $server.BeginInvoke()
    $port = $listener.LocalEndpoint.Port
    $h1 = $null; $h2 = $null
    try { $h1 = Invoke-Hop ([uri]"http://127.0.0.1:$port/a") $dl; $h2 = Invoke-Hop ([uri]"http://127.0.0.1:$port/b") "$dl.2" }
    catch { Write-Host "hop: $($_.Exception.Message)" }
    Check ($h1.status -eq 302 -and $h1.location -eq 'https://example.invalid/next' -and -not (Test-Path $dl)) "real hop returns a redirect without following it"
    Check ($h2.status -eq 200 -and (Test-Path "$dl.2") -and [IO.File]::ReadAllText("$dl.2") -eq 'body') "real hop writes a 200 body"
} finally {
    if ($listener) { $listener.Stop() }
    if ($server) { $server.Dispose() }
    Remove-Item -LiteralPath $dl, "$dl.2" -Force -ErrorAction SilentlyContinue
}

$before = UserPath
$savedEnv = @{ Path = $env:Path; USERPROFILE = $env:USERPROFILE; CODEX_HOME = $env:CODEX_HOME
    OUTLOOK_INDEX_DEBUG = $env:OUTLOOK_INDEX_DEBUG }
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
    Check ($r.attestation_reason -eq 'not_applicable' -and $null -eq $r.attestation) "-FromDir: attestation_reason not_applicable, no attestation"
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
    $ghOnPath = [bool](Get-Command gh -CommandType Application -ErrorAction SilentlyContinue)
    $want = if ($ghOnPath) { 'gh_not_logged_in' } else { 'gh_missing' }
    Check ($code -eq 0 -and $r.sha256 -eq 'verified' -and $r.attestation -eq 'unavailable' -and
        $r.attestation_reason -eq $want) "release install verified (attestation_reason $($r.attestation_reason), want $want)"

    # attestation_reason per gh state: PATH without gh, then a fake gh (auth / attestation exit codes from env).
    $ghDirs = @(Get-Command gh -All -CommandType Application -ErrorAction SilentlyContinue | ForEach-Object { (Split-Path $_.Source).TrimEnd('\') })
    $pathBefore = $env:Path
    $env:Path = @($env:Path -split ';' | Where-Object { $_ -and $ghDirs -notcontains $_.TrimEnd('\') }) -join ';'
    try {
        Check (-not (Get-Command gh -CommandType Application -ErrorAction SilentlyContinue)) "PATH without gh"
        $r = Run $install @('-Version', 'v0.0.1', '-BaseUrl', $base, '-Json')
        Check ($code -eq 0 -and $r.attestation -eq 'unavailable' -and $r.attestation_reason -eq 'gh_missing') "no gh: attestation unavailable, reason gh_missing"
        $fakeGh = Join-Path $tmp 'fakegh'
        New-Item -ItemType Directory -Force -Path $fakeGh | Out-Null
        Set-Content -Path "$fakeGh\gh.cmd" -Encoding Ascii -Value @('@echo off',
            'if "%1"=="auth" exit /b %FAKE_GH_AUTH%', 'if "%1"=="attestation" exit /b %FAKE_GH_VERIFY%', 'exit /b 2')
        $env:Path = "$fakeGh;$env:Path"
        $env:FAKE_GH_AUTH = '1'; $env:FAKE_GH_VERIFY = '0'
        $r = Run $install @('-Version', 'v0.0.1', '-BaseUrl', $base, '-Json')
        Check ($code -eq 0 -and $r.attestation -eq 'unavailable' -and $r.attestation_reason -eq 'gh_not_logged_in') "gh logged out: reason gh_not_logged_in"
        $env:FAKE_GH_AUTH = '0'
        $r = Run $install @('-Version', 'v0.0.1', '-BaseUrl', $base, '-Json')
        Check ($code -eq 0 -and $r.attestation -eq 'verified' -and $r.attestation_reason -eq 'verified') "gh attestation ok: reason verified"
        Run $install @('-Uninstall', '-Json') | Out-Null
        $env:FAKE_GH_VERIFY = '1'
        $r = Run $install @('-Version', 'v0.0.1', '-BaseUrl', $base, '-Json')
        Check ($code -ne 0 -and $r.ok -eq $false -and $r.attestation -eq 'failed' -and $r.attestation_reason -eq 'failed' -and
            -not (Test-Path "$dir\outlook-cli.exe")) "gh attestation fails: reason failed, nothing installed"
    } finally { $env:Path = $pathBefore; $env:FAKE_GH_AUTH = $null; $env:FAKE_GH_VERIFY = $null }
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

    # -PurgeData needs -Yes; checked with the documented Windows PowerShell 5.1 -File command line, then this edition.
    $r = Run $install @('-Uninstall', '-PurgeData', '-Json')
    Check ($code -ne 0 -and (Test-Path "$data\index.sqlite")) "-PurgeData without -Yes refused"
    Push-Location $root
    try { $out = powershell -NoProfile -ExecutionPolicy Bypass -File scripts\install.ps1 -Uninstall -PurgeData -Yes -Json; $code = $LASTEXITCODE }
    finally { Pop-Location }
    $r = $null; try { $r = ($out | Out-String).Trim() | ConvertFrom-Json } catch { Write-Host "not JSON: $out" }
    Check ($code -eq 0 -and $r.data_purged -eq $true -and -not (Test-Path $data)) "powershell -File -PurgeData -Yes removes the data"
    New-Item -ItemType Directory -Force -Path $data | Out-Null
    Set-Content -Path "$data\index.sqlite" -Value 'x'
    $r = Run $install @('-Uninstall', '-PurgeData', '-Yes', '-Json')
    Check ($code -eq 0 -and $r.data_purged -eq $true -and -not (Test-Path $data)) "-PurgeData -Yes removes the data"

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

    # Fake outlook-index: notes each launch and an inherited OUTLOOK_INDEX_DEBUG, prints an error carrying a marker.
    $marker = 'MARKER' + [guid]::NewGuid().ToString('N')
    $fakeExeDir = Join-Path $tmp 'fakeexe'
    New-Item -ItemType Directory -Force -Path $fakeExeDir | Out-Null
    $fakeExe = Join-Path $fakeExeDir 'outlook-index.cmd'
    Set-Content -Path $fakeExe -Encoding Ascii -Value @(
        '@echo off', 'echo x>> "%~dp0launched.txt"', 'if defined OUTLOOK_INDEX_DEBUG echo x> "%~dp0inherited.txt"',
        'powershell -NoProfile -Command "[Diagnostics.Process]::GetCurrentProcess().PriorityClass" > "%~dp0priority.txt"',
        "echo {`"warning`":{`"kind`":`"onedrive`",`"message`":`"$marker`"}} 1>&2",
        "echo {`"error`":{`"kind`":`"io`",`"message`":`"$marker`",`"debug`":`"$marker`"}} 1>&2", 'exit /b 3')

    # -Run -WhatIf: plan only, no launch, no log change (the plan returns before the priority change too).
    $snap = Snapshot $logs
    $r = Run $schedule @('-Run', '-Exe', $fakeExe, '-LogDir', $logs, '-WhatIf', '-Json')
    Check ($code -eq 0 -and $r.what_if -eq $true -and $r.action -eq 'run' -and @($r.planned).Count -eq 1) "-Run -WhatIf returns the plan"
    Check (-not (Test-Path "$fakeExeDir\launched.txt")) "-Run -WhatIf launches nothing"
    Check ((Snapshot $logs) -eq $snap) "-Run -WhatIf writes, rotates and deletes no log"

    # OUTLOOK_INDEX_DEBUG set in the parent: not inherited by the sync; message and debug text never kept.
    $env:OUTLOOK_INDEX_DEBUG = '1'
    try { $r = Run $schedule @('-Run', '-Exe', $fakeExe, '-LogDir', $logs, '-Json'); $runRaw = $script:raw }
    finally { $env:OUTLOOK_INDEX_DEBUG = $savedEnv.OUTLOOK_INDEX_DEBUG }
    $newest = @(Get-ChildItem $logs -Filter 'sync-*.json' | Sort-Object Name)[-1]
    $logRaw = [IO.File]::ReadAllText($newest.FullName)
    Check ((Test-Path "$fakeExeDir\launched.txt") -and -not (Test-Path "$fakeExeDir\inherited.txt")) "sync child does not inherit OUTLOOK_INDEX_DEBUG"
    Check ($code -eq 3 -and $r.error.error.kind -eq 'io') "run keeps the error kind and exit code"
    $prio = if (Test-Path "$fakeExeDir\priority.txt") { "$(Get-Content "$fakeExeDir\priority.txt")".Trim() }
    Check ($prio -eq 'BelowNormal') "sync child runs below normal priority (got '$prio')"
    $r = Run $schedule @('-Status', '-Json')
    Check ($code -eq 0 -and $r.last_run.error.error.kind -eq 'io') "status reports the error kind"
    foreach ($t in @(@('log', $logRaw), @('-Run -Json', $runRaw), @('-Status -Json', $script:raw))) {
        Check ($t[1] -and $t[1] -notmatch $marker -and $t[1] -notmatch '"debug"' -and $t[1] -notmatch '"message"') "$($t[0]): no message, debug or marker"
    }

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

    # Usage skill. Fake hermes: a .cmd (native argument passing, as a real hermes.exe) running a script that keeps
    # skills.external_dirs as JSON in $env:FAKE_HERMES_DIRS and reads `config set` values as single-quoted YAML.
    $fake = Join-Path $tmp 'fakebin'
    New-Item -ItemType Directory -Force -Path $fake | Out-Null
    Set-Content -Path "$fake\hermes.cmd" -Encoding Ascii -Value '@powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0hermes-fake.ps1" %*'
    Set-Content -Path "$fake\hermes-fake.ps1" -Encoding Ascii -Value @'
if ($args[0] -ne 'config' -or $args[2] -ne 'skills.external_dirs') { exit 2 }
if ($args[1] -eq 'get') { [Console]::Out.Write([IO.File]::ReadAllText($env:FAKE_HERMES_DIRS)); exit 0 }
if ($args[1] -ne 'set') { exit 2 }
$items = @([regex]::Matches($args[3], "'((?:[^']|'')*)'") | ForEach-Object { $_.Groups[1].Value -replace "''", "'" })
[IO.File]::WriteAllText($env:FAKE_HERMES_DIRS, (ConvertTo-Json -InputObject $items -Compress))
'Set skills.external_dirs'
'@
    $env:FAKE_HERMES_DIRS = Join-Path $tmp 'hermes-dirs.json'
    [IO.File]::WriteAllText($env:FAKE_HERMES_DIRS, '["C:\\team skills"]')
    function HermesDirs { @([IO.File]::ReadAllText($env:FAKE_HERMES_DIRS) | ConvertFrom-Json | ForEach-Object { $_ }) }
    $env:USERPROFILE = Join-Path $tmp 'home'
    $env:CODEX_HOME = Join-Path $tmp 'codex'
    $claude = Join-Path $tmp 'home\.claude\skills'
    $codex = Join-Path $tmp 'codex\skills'

    $r = Run $install @('-FromDir', $ExeDir, '-Skill', 'hermes', '-Json')
    Check ($code -ne 0 -and $r.ok -eq $false -and -not (Test-Path "$dir\outlook-cli.exe")) "-Skill hermes without hermes refused before installing"
    $r = Run $install @('-FromDir', $ExeDir, '-Skill', 'cursor', '-Json')
    Check ($code -ne 0) "unknown -Skill rejected"
    $env:Path = "$fake;$env:Path"
    $r = Run $install @('-FromDir', $ExeDir, '-Skill', 'hermes', '-Json')
    $d = @(HermesDirs)
    Check ($code -eq 0 -and $r.hermes_registered -eq $true -and (Test-Path "$dir\skills\outlook-cli\SKILL.md") -and
        (Test-Path "$dir\skills\outlook-cli\references\outlook-index.md")) "-Skill hermes copies the skill"
    Check ($d.Count -eq 2 -and $d[0] -eq 'C:\team skills' -and $d[1] -eq "$dir\skills") "hermes external_dirs: $($d -join ' | ')"
    $r = Run $install @('-FromDir', $ExeDir, '-Skill', 'hermes', '-Json')
    Check ($code -eq 0 -and $r.hermes_registered -eq $false -and @(HermesDirs).Count -eq 2) "-Skill hermes is idempotent"
    $r = Run $install @('-FromDir', $ExeDir, '-Skill', 'claude', '-Json')
    Check ($code -eq 0 -and (Test-Path "$claude\outlook-cli\SKILL.md")) "-Skill claude installs in %USERPROFILE%\.claude\skills"
    New-Item -ItemType Directory -Force -Path "$claude\other" | Out-Null
    Set-Content -Path "$claude\other\SKILL.md" -Value 'mine'
    New-Item -ItemType Directory -Force -Path "$codex\outlook-cli" | Out-Null
    Set-Content -Path "$codex\outlook-cli\SKILL.md" -Value 'mine'
    $r = Run $install @('-FromDir', $ExeDir, '-Skill', 'codex', '-Json')
    Check ($code -ne 0 -and (Get-Content "$codex\outlook-cli\SKILL.md") -eq 'mine') "unmanaged skill directory left alone"
    Remove-Item -Recurse -Force "$codex\outlook-cli"
    $r = Run $install @('-FromDir', $ExeDir, '-Skill', 'codex', '-Json')
    Check ($code -eq 0 -and (Test-Path "$codex\outlook-cli\SKILL.md")) "-Skill codex installs in `$CODEX_HOME\skills"

    $r = Run $install @('-Uninstall', '-Json')
    $d = @(HermesDirs)
    Check ($code -eq 0 -and (@($r.skills_removed) -join ',') -eq 'hermes,codex,claude') "uninstall removes the skills: $(@($r.skills_removed) -join ',')"
    Check ($d.Count -eq 1 -and $d[0] -eq 'C:\team skills') "hermes external_dirs back to the unrelated entry"
    Check (-not (Test-Path "$claude\outlook-cli") -and -not (Test-Path "$codex\outlook-cli") -and
        (Get-Content "$claude\other\SKILL.md") -eq 'mine') "managed copies removed, unrelated skill kept"
    $r = Run $install @('-Uninstall', '-Json')
    Check ($code -eq 0 -and @($r.skills_removed).Count -eq 0 -and @(HermesDirs).Count -eq 1) "second uninstall leaves the skills alone"

    # external_dirs unset (null): 0 -> 1 entry (ours) -> 0 entries.
    [IO.File]::WriteAllText($env:FAKE_HERMES_DIRS, 'null')
    $r = Run $install @('-FromDir', $ExeDir, '-Skill', 'hermes', '-Json')
    $d = @(HermesDirs)
    Check ($code -eq 0 -and $r.hermes_registered -eq $true -and $d.Count -eq 1 -and $d[0] -eq "$dir\skills") "hermes external_dirs from none to ours: $($d -join ' | ')"
    $r = Run $install @('-Uninstall', '-Json')
    Check ($code -eq 0 -and (@($r.skills_removed) -join ',') -eq 'hermes' -and @(HermesDirs).Count -eq 0) "hermes external_dirs back to none"
} finally {
    foreach ($k in $savedEnv.Keys) { [Environment]::SetEnvironmentVariable($k, $savedEnv[$k]) } # $null unsets
    Unregister-ScheduledTask -TaskName $task -Confirm:$false -ErrorAction SilentlyContinue
    if ((UserPath) -ne $before) { [Environment]::SetEnvironmentVariable('Path', $before, 'User') }
    Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
}
if ($script:fails) { Write-Host "$($script:fails) failure(s)"; exit 1 }
Write-Host "install.ps1 / schedule.ps1 / build.ps1: all checks passed ($($PSVersionTable.PSEdition) $($PSVersionTable.PSVersion))"
exit 0
