$ErrorActionPreference = "Stop"
$ScriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$RootDir = Split-Path -Parent $ScriptDir
$RustDir = Join-Path $ScriptDir "rust-msg3-parser"
$Manifest = Join-Path $RustDir "Cargo.toml"

. (Join-Path $ScriptDir "load_env.ps1")

if ($args.Count -lt 1) {
    throw "Usage: .\qq-analyzer\run_rust_tool_windows.ps1 <rust-bin> [args...]"
}

$bin = $args[0]
$toolArgs = @()
if ($args.Count -gt 1) {
    $toolArgs = @($args[1..($args.Count - 1)])
}

Set-Location $RootDir

$cargo = Get-Command cargo -ErrorAction SilentlyContinue
if (-not $cargo) {
    throw "Rust cargo was not found in PATH."
}

& $cargo.Source run --manifest-path $Manifest --bin $bin -- @toolArgs
exit $LASTEXITCODE
