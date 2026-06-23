$ErrorActionPreference = "Stop"
$ScriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$RootDir = Split-Path -Parent $ScriptDir
$RustDir = Join-Path $ScriptDir "rust-msg3-parser"
$Manifest = Join-Path $RustDir "Cargo.toml"

. (Join-Path $ScriptDir "load_env.ps1")
Set-Location $RootDir

$cargo = Get-Command cargo -ErrorAction SilentlyContinue
if (-not $cargo) {
    throw "Rust cargo was not found in PATH."
}

& $cargo.Source run --manifest-path $Manifest --bin qq_analyzer_rs -- serve @args
exit $LASTEXITCODE
