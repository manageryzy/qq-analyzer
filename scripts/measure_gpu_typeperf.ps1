param(
    [ValidateSet("directml", "cuda")]
    [string]$Ep = "directml",
    [ValidateSet("manifest", "embeddings", "all")]
    [string]$Stage = "all",
    [int]$Limit = 1000,
    [int]$BatchSize = 32,
    [int]$Samples = 60,
    [string]$Account = "",
    [string]$Root = "",
    [string[]]$AssetRoot = @(),
    [string]$OutputRoot = ""
)

$ErrorActionPreference = "Stop"

function Quote-Argument([string]$Arg) {
    if ($null -eq $Arg -or $Arg.Length -eq 0) {
        return '""'
    }
    return '"' + $Arg.Replace('"', '\"') + '"'
}

function To-IntOrZero($Value) {
    if ($null -eq $Value -or "$Value" -eq "") {
        return 0
    }
    return [int]$Value
}

$scriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$repo = Resolve-Path (Join-Path $scriptDir "..")
$repoPath = $repo.Path
$exe = Join-Path $repoPath "target\release\qq_analyzer_rs.exe"
if (-not (Test-Path $exe)) {
    throw "Missing release executable: $exe"
}

if ([string]::IsNullOrWhiteSpace($Account)) {
    $Account = "pid_gpu_${Ep}_typeperf"
}

if ($Ep -eq "cuda") {
    $cudaDllDirs = @(
        (Join-Path $repoPath "output\_deps\nvidia-cu12-win\nvidia\cudnn\bin"),
        (Join-Path $repoPath "output\_deps\nvidia-cu12-win\nvidia\cublas\bin"),
        (Join-Path $repoPath "output\_deps\nvidia-cu12-win\nvidia\cuda_nvrtc\bin"),
        "C:\Program Files\NVIDIA GPU Computing Toolkit\CUDA\v12.5\bin"
    ) | Where-Object { Test-Path $_ }
    if ($cudaDllDirs.Count -gt 0) {
        $env:PATH = ($cudaDllDirs -join ";") + ";" + $env:PATH
    }
}

$defaultRunRoot = Join-Path $repoPath "output\_image_index_pid_gpu_${Ep}_typeperf"
$runRoot = if ([string]::IsNullOrWhiteSpace($Root)) { $defaultRunRoot } else { $Root }
$reportRoot = if ([string]::IsNullOrWhiteSpace($OutputRoot)) { $runRoot } else { $OutputRoot }
if ($AssetRoot.Count -eq 0) {
    $AssetRoot = @(Join-Path $repoPath "output\_image_index_bench_5000\fixtures")
}
$modelDir = Join-Path $repoPath "output\_deps\models\mobileclip2-s2"
$sscdDir = Join-Path $repoPath "output\_deps\models\sscd"
$out = Join-Path $reportRoot "build_${Ep}_${Stage}_${Limit}.json"
$stdout = Join-Path $reportRoot "stdout_${Ep}_${Stage}_${Limit}.txt"
$stderr = Join-Path $reportRoot "stderr_${Ep}_${Stage}_${Limit}.txt"
$typeperfCsv = Join-Path $reportRoot "typeperf_${Ep}_${Stage}_${Limit}.csv"
New-Item -ItemType Directory -Force -Path $reportRoot | Out-Null

$argsList = @(
    "image-index", "build",
    "--root", $runRoot,
    "--account", $Account,
    "--pipeline", "full",
    "--stage", $Stage,
    "--model-dir", $modelDir,
    "--sscd-model-dir", $sscdDir,
    "--ep", $Ep,
    "--clip-batch-size", "$BatchSize",
    "--limit", "$Limit",
    "--force",
    "--out", $out
)
foreach ($asset in $AssetRoot) {
    $argsList += @("--asset-root", $asset)
}
$argLine = ($argsList | ForEach-Object { Quote-Argument $_ }) -join " "

$proc = Start-Process -FilePath $exe `
    -ArgumentList $argLine `
    -WorkingDirectory $repoPath `
    -RedirectStandardOutput $stdout `
    -RedirectStandardError $stderr `
    -PassThru

Write-Host "Started qq_analyzer_rs.exe PID=$($proc.Id) EP=$Ep limit=$Limit batch=$BatchSize"

Start-Sleep -Seconds 2

$typeperfArgs = @(
    "\GPU Engine(*)\Utilization Percentage",
    "-si", "1",
    "-sc", "$Samples",
    "-f", "CSV",
    "-o", $typeperfCsv
)
$typeperfArgLine = ($typeperfArgs | ForEach-Object { Quote-Argument $_ }) -join " "
$typeperf = Start-Process -FilePath "typeperf.exe" `
    -ArgumentList $typeperfArgLine `
    -WindowStyle Hidden `
    -PassThru

$proc.WaitForExit()
$proc.Refresh()
$rawExitCode = $proc.ExitCode

if (-not $typeperf.HasExited) {
    Start-Sleep -Seconds 2
}
if (-not $typeperf.HasExited) {
    Stop-Process -Id $typeperf.Id -Force -ErrorAction SilentlyContinue
}

$rows = @()
$pidPattern = "pid_$($proc.Id)_"
if (Test-Path $typeperfCsv) {
    $csvRows = Import-Csv -Path $typeperfCsv
    if ($csvRows.Count -gt 0) {
        $headers = $csvRows[0].PSObject.Properties.Name |
            Where-Object { $_ -match [regex]::Escape($pidPattern) }
        foreach ($header in $headers) {
            $values = foreach ($row in $csvRows) {
                $raw = $row.$header
                $parsed = 0.0
                if ([double]::TryParse($raw, [ref]$parsed)) {
                    $parsed
                }
            }
            if ($values.Count -gt 0) {
                $engineType = ""
                if ($header -match "engtype_([^)\\]+)") {
                    $engineType = $matches[1]
                }
                $rows += [PSCustomObject]@{
                    EngineType = $engineType
                    Counter = $header
                    Samples = $values.Count
                    MaxUtil = [Math]::Round(($values | Measure-Object -Maximum).Maximum, 2)
                    AvgUtil = [Math]::Round(($values | Measure-Object -Average).Average, 2)
                    ActiveSamples = @($values | Where-Object { $_ -gt 0.1 }).Count
                }
            }
        }
    }
}

$report = $null
if (Test-Path $out) {
    $report = Get-Content -Raw -Path $out | ConvertFrom-Json
}

$exitCode = $rawExitCode
$exitCodeSource = "process"
if ($null -eq $exitCode -or "$exitCode" -eq "") {
    $exitCodeSource = "inferred_from_report"
    $successfulItems = 0
    if ($null -ne $report) {
        $successfulItems += To-IntOrZero $report.indexed_files
        $successfulItems += To-IntOrZero $report.embedded_files
        $successfulItems += To-IntOrZero $report.reused_embedding_files
        $successfulItems += To-IntOrZero $report.unchanged_files
    }
    if ($null -ne $report -and $report.error_files -eq 0 -and $successfulItems -gt 0) {
        $exitCode = 0
    } else {
        $exitCode = 1
    }
}

Write-Host "ExitCode=$exitCode ($exitCodeSource)"
Write-Host "TypeperfCsv=$typeperfCsv"
Write-Host "MatchedCounters=$($rows.Count)"
if ($rows.Count -gt 0) {
    $rows |
        Sort-Object MaxUtil -Descending |
        Select-Object -First 12 EngineType,Samples,ActiveSamples,MaxUtil,AvgUtil |
        Format-Table -AutoSize
}

if ($null -ne $report) {
    Write-Host "Report scanned_files=$($report.scanned_files) indexed_files=$($report.indexed_files) embedded_files=$($report.embedded_files) reused_embedding_files=$($report.reused_embedding_files) error_files=$($report.error_files)"
    if ($report.elapsed_ms) {
        Write-Host "Report elapsed_ms=$($report.elapsed_ms)"
    }
    if ($report.model_status) {
        Write-Host "ProviderStatus=$($report.model_status.execution_provider_status)"
        Write-Host "Semantic=$($report.model_status.semantic_descriptor)"
        Write-Host "Copy=$($report.model_status.copy_descriptor)"
    }
}

Write-Host "Out=$out"
Write-Host "Stdout=$stdout"
Write-Host "Stderr=$stderr"

exit $exitCode
