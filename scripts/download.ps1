# Download policy shared by scripts\install.ps1 and scripts\build.ps1 (dot-sourced; defines functions only).
# Windows PowerShell 5.1 and 7. Redirects are followed by hand, at most 10 (11 requests in all): every URL, the
# first one and each redirect target, must be https before it is requested, so an https -> http downgrade is refused
# without a plain-http request. Callers verify the SHA-256 of what was downloaded before running or installing it.

# Saves $url to $dest: at most 1 request + 10 redirects (11 requests); the 11th redirect is refused, not requested.
# $Fetch makes one request without following redirects and returns @{ status = <int>; location = <Location header> },
# writing the body to $dest on 2xx (tests pass a fake). On any failure $dest is removed (no partial file is left).
function Save-Https([string]$url, [string]$dest, [scriptblock]$Fetch = ${function:Invoke-Hop}) {
    try {
        $uri = [uri]$url
        $redirects = 0
        while ($true) {
            if (-not $uri.IsAbsoluteUri -or $uri.Scheme -ne 'https') { throw 'download refused: not an https URL' }
            $r = & $Fetch $uri $dest
            if ($r.status -ge 200 -and $r.status -lt 300) { return }
            if (@(301, 302, 303, 307, 308) -notcontains $r.status -or -not $r.location) { throw "download failed (HTTP $($r.status))" }
            $redirects++
            if ($redirects -gt 10) { throw 'download refused: more than 10 redirects' }
            $uri = New-Object Uri($uri, [string]$r.location) # relative Location: resolved against the current URL
        }
    } catch {
        Remove-Item -LiteralPath $dest -Force -ErrorAction SilentlyContinue
        throw
    }
}

# One request, redirects not followed (same behaviour in .NET Framework and .NET).
function Invoke-Hop([uri]$uri, [string]$dest) {
    $q = [Net.WebRequest]::Create($uri)
    $q.AllowAutoRedirect = $false
    $q.Timeout = [Threading.Timeout]::Infinite # no overall limit, as Invoke-WebRequest's default (~190 MB llvm-mingw)
    try { $r = $q.GetResponse() }
    catch {
        # 4xx/5xx (and 3xx on some runtimes) throw a WebException that carries the response.
        $e = $_.Exception
        while ($e -and -not ($e -is [Net.WebException])) { $e = $e.InnerException }
        if (-not $e -or -not $e.Response) { throw }
        $r = $e.Response
    }
    try {
        $s = [int]$r.StatusCode
        if ($s -ge 200 -and $s -lt 300) {
            $f = [IO.File]::Create($dest)
            try { $r.GetResponseStream().CopyTo($f) } finally { $f.Close() }
        }
        @{ status = $s; location = $r.Headers['Location'] }
    } finally { $r.Close() }
}
