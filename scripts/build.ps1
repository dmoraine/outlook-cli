<#
.SYNOPSIS
Native source build on Windows, without WSL, Visual Studio or admin rights, then scripts\install.ps1 -FromDir dist.
Windows PowerShell 5.1 and 7.

.DESCRIPTION
Builds for x86_64-pc-windows-gnullvm with Rust's stable-x86_64-pc-windows-gnullvm toolchain (the user's default
toolchain is left alone) and a portable llvm-mingw (pinned version, SHA-256 checked) in
%LOCALAPPDATA%\outlook-cli\toolchain\. Outputs dist\outlook-cli.exe and dist\outlook-index.exe.
  -InstallToolchain  consent to download what is missing: rustup for the user (%USERPROFILE%\.cargo, adds its bin to
                     the user PATH), the gnullvm toolchain (minimal profile), llvm-mingw (~190 MB zip). Without it,
                     the script stops and lists what is missing.
  -NoTests           build only (CI runs the tests).
  -Jobs N            CARGO_BUILD_JOBS (default: $env:CARGO_BUILD_JOBS, else 2); -Jobs 1 is slower and uses the least RAM.
Disk: about 3 GB with -NoTests (toolchains ~1.4 GB, target\ ~1.5 GB), about 5 GB with the tests (their LTO-off
build adds ~2 GB). RAM: about 8 GB free with 2 jobs (the fat-LTO release link is the peak); -Jobs 1 if it runs out.
-WhatIf lists the steps without running them. -Json prints one JSON object on stdout (cargo output goes to stderr).

.EXAMPLE
powershell -ExecutionPolicy Bypass -File scripts\build.ps1 -InstallToolchain -NoTests -Json
#>
[CmdletBinding(SupportsShouldProcess = $true)]
param(
    [switch]$InstallToolchain,
    [switch]$NoTests,
    [ValidateRange(1, 64)][int]$Jobs = $(if ($env:CARGO_BUILD_JOBS -match '^\d+$') { [int]$env:CARGO_BUILD_JOBS } else { 2 }),
    [switch]$Json
)
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
$Cmdlet = $PSCmdlet
$Triple = 'x86_64-pc-windows-gnullvm'
$Toolchain = "stable-$Triple"
# Pinned llvm-mingw (same release as scripts/setup-toolchain.sh users get today); bump both lines together.
$LlvmName = 'llvm-mingw-20261117-ucrt-x86_64'
$LlvmSha256 = '4d002e330a0f703625aa5ccbc37e96f8f824413e163bfc7336774aabd2c3a412'
$LlvmUrl = "https://github.com/mstorsjo/llvm-mingw/releases/download/20261117/$LlvmName.zip"
$RustupUrl = 'https://static.rust-lang.org/rustup/dist/x86_64-pc-windows-msvc/rustup-init.exe'
$script:planned = @()

function Want([string]$target, [string]$action) {
    if ($WhatIfPreference) { $script:planned += "$action $target"; return $false }
    $Cmdlet.ShouldProcess($target, $action)
}
# Runs a native command; its stdout goes to stderr so that -Json output stays clean.
function Exec([string]$exe, [string[]]$arguments) {
    $ErrorActionPreference = 'Continue' # native stderr must not throw in 5.1
    & $exe @arguments | ForEach-Object { [Console]::Error.WriteLine($_) }
    if ($LASTEXITCODE -ne 0) { throw "$(Split-Path $exe -Leaf) $($arguments[0..1] -join ' ') failed (exit $LASTEXITCODE)" }
}
function Get-Verified([string]$url, [string]$sha256, [string]$dest) {
    Invoke-WebRequest -UseBasicParsing -Uri $url -OutFile $dest
    if ((Get-FileHash -LiteralPath $dest -Algorithm SHA256).Hash -ine $sha256) {
        Remove-Item -LiteralPath $dest -Force
        throw "SHA-256 mismatch for $(Split-Path $url -Leaf)"
    }
}

try {
    if (-not $env:LOCALAPPDATA) { throw 'LOCALAPPDATA is not set' }
    $root = Split-Path $PSScriptRoot
    $tc = Join-Path $env:LOCALAPPDATA 'outlook-cli\toolchain'
    $llvm = Join-Path $tc $LlvmName
    $clang = Join-Path $llvm 'bin\x86_64-w64-mingw32-clang.exe'
    $cargoHome = if ($env:CARGO_HOME) { $env:CARGO_HOME } else { Join-Path $env:USERPROFILE '.cargo' }
    $rustup = Get-Command rustup -ErrorAction SilentlyContinue | Select-Object -First 1 -ExpandProperty Source
    if (-not $rustup -and (Test-Path -LiteralPath (Join-Path $cargoHome 'bin\rustup.exe'))) { $rustup = Join-Path $cargoHome 'bin\rustup.exe' }

    $missing = @()
    if (-not $rustup) { $missing += 'rustup' }
    if (-not $rustup -or -not ((& $rustup toolchain list) -match "^$([regex]::Escape($Toolchain))")) { $missing += $Toolchain }
    if (-not (Test-Path -LiteralPath $clang)) { $missing += $LlvmName }
    if ($missing.Count -and -not $InstallToolchain -and -not $WhatIfPreference) {
        throw "missing: $($missing -join ', '); rerun with -InstallToolchain to download them (user-only, no admin)"
    }
    [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12

    if ($missing -contains 'rustup' -and (Want $cargoHome 'Install rustup (user)')) {
        $init = Join-Path ([IO.Path]::GetTempPath()) ('rustup-init-' + [guid]::NewGuid().ToString('N') + '.exe')
        try {
            Invoke-WebRequest -UseBasicParsing -Uri "$RustupUrl.sha256" -OutFile "$init.sha256"
            $sum = ((Get-Content -LiteralPath "$init.sha256" -Raw) -split '\s+')[0]
            Get-Verified $RustupUrl $sum $init
            # A gnullvm host needs no Visual Studio (rustup only asks for it with an msvc host).
            Exec $init @('-y', '--profile', 'minimal', '--default-host', $Triple, '--default-toolchain', 'stable')
        } finally { Remove-Item -LiteralPath $init, "$init.sha256" -Force -ErrorAction SilentlyContinue }
        $rustup = Join-Path $cargoHome 'bin\rustup.exe'
    }
    if ($missing -contains $Toolchain -and (Want $Toolchain 'Install Rust toolchain')) {
        Exec $rustup @('toolchain', 'install', $Toolchain, '--profile', 'minimal')
    }
    if ($missing -contains $LlvmName -and (Want $llvm 'Download and unpack llvm-mingw')) {
        New-Item -ItemType Directory -Force -Path $tc | Out-Null
        $zip = Join-Path $tc "$LlvmName.zip"
        $tmp = Join-Path $tc '.unpack'
        try {
            Get-Verified $LlvmUrl $LlvmSha256 $zip
            if (Test-Path -LiteralPath $tmp) { Remove-Item -LiteralPath $tmp -Recurse -Force }
            Add-Type -AssemblyName System.IO.Compression.FileSystem
            [IO.Compression.ZipFile]::ExtractToDirectory($zip, $tmp)
            if (Test-Path -LiteralPath $llvm) { Remove-Item -LiteralPath $llvm -Recurse -Force }
            Move-Item -LiteralPath (Join-Path $tmp $LlvmName) -Destination $llvm
        } finally {
            Remove-Item -LiteralPath $zip, $tmp -Recurse -Force -ErrorAction SilentlyContinue
        }
    }

    $env:Path = "$(Join-Path $llvm 'bin');$env:Path"
    $env:CARGO_BUILD_JOBS = "$Jobs"
    $env:CARGO_TARGET_X86_64_PC_WINDOWS_GNULLVM_LINKER = $clang
    $env:CC_x86_64_pc_windows_gnullvm = $clang # bundled SQLite (feature "index") is C
    $env:AR_x86_64_pc_windows_gnullvm = Join-Path $llvm 'bin\llvm-ar.exe'
    Push-Location $root
    try {
        if (-not $NoTests -and (Want $root 'cargo test (with and without --features index)')) {
            # Tests only: no LTO and parallel codegen use far less RAM than the release profile.
            $env:CARGO_PROFILE_RELEASE_LTO = 'off'; $env:CARGO_PROFILE_RELEASE_CODEGEN_UNITS = '16'
            try {
                Exec $rustup @('run', $Toolchain, 'cargo', 'test', '--release', '--quiet', '--target', $Triple)
                Exec $rustup @('run', $Toolchain, 'cargo', 'test', '--release', '--quiet', '--target', $Triple, '--features', 'index')
            } finally { Remove-Item Env:CARGO_PROFILE_RELEASE_LTO, Env:CARGO_PROFILE_RELEASE_CODEGEN_UNITS -ErrorAction SilentlyContinue }
        }
        $dist = Join-Path $root 'dist'
        if (Want $dist 'cargo build --release (outlook-cli.exe, outlook-index.exe)') {
            Exec $rustup @('run', $Toolchain, 'cargo', 'build', '--release', '--target', $Triple)
            Exec $rustup @('run', $Toolchain, 'cargo', 'build', '--release', '--target', $Triple, '--features', 'index', '--bin', 'outlook-index')
            New-Item -ItemType Directory -Force -Path $dist | Out-Null
            foreach ($e in 'outlook-cli.exe', 'outlook-index.exe') {
                Copy-Item -LiteralPath (Join-Path $root "target\$Triple\release\$e") -Destination $dist -Force
            }
        }
    } finally { Pop-Location }

    $o = [ordered]@{ ok = $true; action = 'build'; target = $Triple; toolchain = $Toolchain; llvm_mingw = $LlvmName
        jobs = $Jobs; tests = $(if ($NoTests) { 'skipped' } else { 'run' }); missing = $missing; dist = $dist }
    if ($WhatIfPreference) {
        $o.tests = 'not_run'; $o.what_if = $true; $o.planned = $script:planned + "scripts\install.ps1 -FromDir $dist"
    } else {
        $installed = & (Join-Path $PSScriptRoot 'install.ps1') -FromDir $dist -Json | ConvertFrom-Json
        if (-not $installed.ok) { throw "install failed: $($installed.error)" }
        $o.install = $installed
    }
    if ($Json) { $o | ConvertTo-Json -Depth 6 -Compress }
    else {
        $script:planned | ForEach-Object { Write-Host "What if: $_" }
        Write-Host (([pscustomobject]$o | Format-List | Out-String).Trim())
    }
} catch {
    $msg = $_.Exception.Message
    if ($Json) { [ordered]@{ ok = $false; error = $msg } | ConvertTo-Json -Compress } else { Write-Error $msg -ErrorAction Continue }
    exit 1
}
