<#
.SYNOPSIS
    Run the P0-5 spike checks, with no Unix shell in the loop.

.DESCRIPTION
    `cargo xtask spike` is the only implementation of the six checks (fmt,
    rustfmt on the helper, clippy, the 23 tests, the helper's 4, and the
    process-level protocol checker), and CI runs that same command. This wrapper
    exists for two reasons: it can be started from PowerShell or cmd.exe on a
    machine where a shell is the easiest entry point, and `-Wsl` runs the same
    command inside the distro, so both halves are one flag apart.

    Nothing here needs the bash bundled with an editor — which is the point:
    Windows may block it, and several agents may run the same checks.

.PARAMETER Wsl
    Also run `cargo xtask spike` inside the distro through wsl.exe.

.PARAMETER WslDistro
    Distro for -Wsl. Defaults to $env:ACPILOT_WSL_DISTRO, because reading
    `wsl.exe -l -q` from PowerShell mangles its UTF-16 output.

.EXAMPLE
    pwsh -File spikes/wsl-helper/check.ps1

.EXAMPLE
    pwsh -File spikes/wsl-helper/check.ps1 -Wsl -WslDistro Ubuntu-24.04
#>
[CmdletBinding()]
param(
    [switch]$Wsl,
    [string]$WslDistro = $env:ACPILOT_WSL_DISTRO
)

$ErrorActionPreference = 'Stop'

$repo = (Resolve-Path (Join-Path $PSScriptRoot '../..')).Path
$onWindows = ($env:OS -eq 'Windows_NT')

$failures = New-Object System.Collections.Generic.List[string]
$skips = New-Object System.Collections.Generic.List[string]

function Invoke-Step {
    param([string]$Name, [scriptblock]$Body)
    Write-Host "==> $Name" -ForegroundColor Cyan
    try {
        & $Body
    }
    catch {
        Write-Host "    error: $($_.Exception.Message)" -ForegroundColor Red
        $failures.Add($Name)
        return
    }
    if ($null -ne $LASTEXITCODE -and $LASTEXITCODE -ne 0) {
        Write-Host "    failed (exit $LASTEXITCODE)" -ForegroundColor Red
        $failures.Add("$Name (exit $LASTEXITCODE)")
    }
}

function Write-Skip {
    param([string]$Name, [string]$Why)
    Write-Host "==> $Name" -ForegroundColor Cyan
    Write-Host "    skipped: $Why" -ForegroundColor Yellow
    $skips.Add("$Name ($Why)")
}

Push-Location $repo
try {
    Invoke-Step 'cargo xtask spike' {
        & cargo xtask spike
    }

    if ($Wsl) {
        if (-not $onWindows) {
            Write-Skip 'distro checks' '-Wsl only applies on Windows'
        }
        elseif (-not $WslDistro) {
            Write-Skip 'distro checks' 'pass -WslDistro or set ACPILOT_WSL_DISTRO'
        }
        else {
            $wslExe = Join-Path $env:SystemRoot 'System32\wsl.exe'
            $linuxRepo = (& $wslExe -d $WslDistro -e wslpath -a $repo | Out-String).Trim()
            if ($LASTEXITCODE -ne 0 -or -not $linuxRepo) {
                Write-Host "==> distro checks in $WslDistro" -ForegroundColor Cyan
                Write-Host "    failed: could not map $repo with wslpath" -ForegroundColor Red
                $failures.Add("map $repo into $WslDistro")
            }
            else {
                # The distro's own bash is the login shell that puts cargo on
                # PATH; it is not an editor's copy.
                Invoke-Step "cargo xtask spike in $WslDistro ($linuxRepo)" {
                    & $wslExe -d $WslDistro --cd $linuxRepo -e bash -lc 'cargo xtask spike'
                }
            }
        }
    }
}
finally {
    Pop-Location
}

Write-Host ''
if ($skips.Count -gt 0) {
    Write-Host 'skipped:' -ForegroundColor Yellow
    foreach ($skip in $skips) { Write-Host "  - $skip" -ForegroundColor Yellow }
}
if ($failures.Count -gt 0) {
    Write-Host 'FAILED:' -ForegroundColor Red
    foreach ($failure in $failures) { Write-Host "  - $failure" -ForegroundColor Red }
    exit 1
}
Write-Host 'all checks passed' -ForegroundColor Green
exit 0
