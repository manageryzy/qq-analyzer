$ErrorActionPreference = "Stop"
$ScriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$RootDir = Split-Path -Parent $ScriptDir
$RustDir = Join-Path $ScriptDir "rust-msg3-parser"
$Manifest = Join-Path $RustDir "Cargo.toml"
$LegacyAnalyzer = Join-Path $ScriptDir "archive\python-legacy\qq_analyzer.py"

. (Join-Path $ScriptDir "load_env.ps1")
Set-Location $RootDir

if ($args.Count -gt 0 -and $args[0] -eq "legacy-python") {
    if (-not (Test-Path -LiteralPath $LegacyAnalyzer)) {
        throw "Archived legacy analyzer was not found: $LegacyAnalyzer"
    }
    $pythonArgs = @()
    if ($args.Count -gt 1) {
        $pythonArgs = @($args[1..($args.Count - 1)])
    }
    $python = Get-Command python -ErrorAction SilentlyContinue
    if (-not $python) {
        $python = Get-Command py -ErrorAction SilentlyContinue
    }
    if (-not $python) {
        throw "Windows Python was not found in PATH."
    }
    Write-Warning "legacy-python is a migration-only path. Prefer .\qq-analyzer\run_windows.ps1 <qq_analyzer_rs args>."
    & $python.Source $LegacyAnalyzer @pythonArgs
    exit $LASTEXITCODE
}

$cargo = Get-Command cargo -ErrorAction SilentlyContinue
if (-not $cargo) {
    throw "Rust cargo was not found in PATH."
}

& $cargo.Source run --manifest-path $Manifest --bin qq_analyzer_rs -- @args
exit $LASTEXITCODE
