$ErrorActionPreference = "Stop"
$ScriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$RootDir = Split-Path -Parent $ScriptDir
$LegacyDir = Join-Path $ScriptDir "archive\python-legacy"

. (Join-Path $ScriptDir "load_env.ps1")

if ($args.Count -lt 1) {
    throw "Usage: .\qq-analyzer\run_windows_script.ps1 <legacy-script-under-qq-analyzer> [args...]"
}

$scriptName = $args[0]
$scriptPath = Join-Path $ScriptDir $scriptName
if (-not (Test-Path -LiteralPath $scriptPath)) {
    $scriptPath = Join-Path $LegacyDir $scriptName
}
if (-not (Test-Path -LiteralPath $scriptPath)) {
    throw "Script was not found under qq-analyzer or archive\python-legacy: $scriptName"
}

Set-Location $RootDir

$python = Get-Command python -ErrorAction SilentlyContinue
if (-not $python) {
    $python = Get-Command py -ErrorAction SilentlyContinue
}
if (-not $python) {
    throw "Windows Python was not found in PATH."
}

Write-Warning "run_windows_script.ps1 is a legacy-only Python wrapper for archived historical probes. Prefer .\qq-analyzer\run_windows.ps1 <qq_analyzer_rs args> for analyzer work."

if ($args.Count -gt 1) {
    & $python.Source $scriptPath @($args[1..($args.Count - 1)])
} else {
    & $python.Source $scriptPath
}
exit $LASTEXITCODE
