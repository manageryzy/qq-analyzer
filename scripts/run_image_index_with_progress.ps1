param(
    [Parameter(Mandatory = $true)]
    [string]$Root,

    [Parameter(Mandatory = $true)]
    [string]$Account,

    [ValidateSet("manifest", "embeddings", "all")]
    [string]$Stage = "all",

    [ValidateSet("auto", "cuda", "directml", "cpu")]
    [string]$Ep = "auto",

    [int]$Limit = 1000000,
    [int]$BatchSize = 16,
    [ValidateSet("full", "fast")]
    [string]$ManifestMode = "fast",
    [int]$ManifestWorkers = 0,
    [int]$PollSeconds = 10,
    [string[]]$AssetRoot = @(),
    [string]$Out = "",
    [switch]$Force
)

$ErrorActionPreference = "Stop"

function Quote-Argument([string]$Arg) {
    if ($null -eq $Arg -or $Arg.Length -eq 0) {
        return '""'
    }
    return '"' + $Arg.Replace('"', '\"') + '"'
}

function Read-Status($Exe, $Root, $Account, $OutPath) {
    & $Exe image-index status --root $Root --account $Account --out $OutPath | Out-Null
    if (Test-Path $OutPath) {
        return Get-Content -Raw -Path $OutPath | ConvertFrom-Json
    }
    return $null
}

function To-IntOrZero($Value) {
    if ($null -eq $Value -or "$Value" -eq "") {
        return 0
    }
    return [int]$Value
}

function Compact-BuildReport($Report) {
    if ($null -eq $Report) {
        return $null
    }
    return [pscustomobject]@{
        stage = $Report.stage
        scanned_files = To-IntOrZero $Report.scanned_files
        indexed_files = To-IntOrZero $Report.indexed_files
        embedded_files = To-IntOrZero $Report.embedded_files
        reused_embedding_files = To-IntOrZero $Report.reused_embedding_files
        unchanged_files = To-IntOrZero $Report.unchanged_files
        skipped_files = To-IntOrZero $Report.skipped_files
        error_files = To-IntOrZero $Report.error_files
        stale_files = To-IntOrZero $Report.stale_files
        elapsed_ms = To-IntOrZero $Report.elapsed_ms
    }
}

function Compact-Status($Status) {
    if ($null -eq $Status) {
        return $null
    }
    return [pscustomobject]@{
        images = To-IntOrZero $Status.images
        stale_images = To-IntOrZero $Status.stale_images
        errors = To-IntOrZero $Status.errors
        active_clip_rows = To-IntOrZero $Status.embeddings.active_clip_rows
        active_sscd_rows = To-IntOrZero $Status.embeddings.active_sscd_rows
        active_images_missing_clip = To-IntOrZero $Status.embeddings.active_images_missing_clip
        active_images_missing_sscd = To-IntOrZero $Status.embeddings.active_images_missing_sscd
        active_images = To-IntOrZero $Status.quality.active_images
        duplicate_sha_groups = To-IntOrZero $Status.quality.duplicate_sha_groups
        duplicate_sha_files = To-IntOrZero $Status.quality.duplicate_sha_files
        exact_phash_groups = To-IntOrZero $Status.quality.exact_phash_groups
        exact_phash_files = To-IntOrZero $Status.quality.exact_phash_files
        blurry_files = To-IntOrZero $Status.quality.blurry_files
        small_files = To-IntOrZero $Status.quality.small_files
        tiny_files = To-IntOrZero $Status.quality.tiny_files
        thumbnail_like_files = To-IntOrZero $Status.quality.thumbnail_like_files
    }
}

function Sample-GpuEngines($TargetProcessId) {
    $hits = @()
    try {
        $counterSamples = (Get-Counter '\GPU Engine(*)\Utilization Percentage').CounterSamples
        $pidPrefix = "pid_${TargetProcessId}_"
        foreach ($sample in $counterSamples) {
            if ($sample.InstanceName.StartsWith($pidPrefix) -and $sample.CookedValue -gt 0.01) {
                $engineType = ""
                if ($sample.InstanceName -match "engtype_([^_\\)]+)") {
                    $engineType = $matches[1]
                }
                $hits += [pscustomobject]@{
                    instance = $sample.InstanceName
                    engine_type = $engineType
                    utilization = [math]::Round($sample.CookedValue, 3)
                }
            }
        }
    } catch {
        return [pscustomobject]@{
            active_engines = 0
            max_utilization = 0
            top_engine = ""
            error = $_.Exception.Message
        }
    }
    if ($hits.Count -eq 0) {
        return [pscustomobject]@{
            active_engines = 0
            max_utilization = 0
            top_engine = ""
            error = ""
        }
    }
    $top = $hits | Sort-Object utilization -Descending | Select-Object -First 1
    return [pscustomobject]@{
        active_engines = $hits.Count
        max_utilization = $top.utilization
        top_engine = $top.engine_type
        error = ""
    }
}

$scriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$repo = Resolve-Path (Join-Path $scriptDir "..")
$repoPath = $repo.Path
$exe = Join-Path $repoPath "target\release\qq_analyzer_rs.exe"
if (-not (Test-Path $exe)) {
    throw "Missing release executable: $exe"
}

if ($Ep -eq "cuda") {
    $cudaRoot = Join-Path $repoPath "output\_deps\nvidia-cu12-win\nvidia"
    $cudaDllDirs = @(
        (Join-Path $cudaRoot "cublas\bin"),
        (Join-Path $cudaRoot "cuda_nvrtc\bin"),
        (Join-Path $cudaRoot "cudnn\bin"),
        "C:\Program Files\NVIDIA GPU Computing Toolkit\CUDA\v12.5\bin"
    ) | Where-Object { Test-Path $_ }
    if ($cudaDllDirs.Count -gt 0) {
        $env:PATH = ($cudaDllDirs -join ";") + ";" + $env:PATH
    }
}

$reportDir = Join-Path $repoPath "output\image-index-progress"
New-Item -ItemType Directory -Force -Path $reportDir | Out-Null
if ([string]::IsNullOrWhiteSpace($Out)) {
    $safeAccount = $Account -replace '[^\w.-]', '_'
    $Out = Join-Path $reportDir "${safeAccount}_${Stage}_${Ep}_build.json"
}
$stdout = "$Out.stdout.txt"
$stderr = "$Out.stderr.txt"
$statusOut = "$Out.status.json"
$summaryOut = "$Out.progress.json"
Remove-Item -Force -ErrorAction SilentlyContinue $stdout, $stderr, $statusOut, $summaryOut

$baselineStatus = $null
try {
    $baselineStatus = Read-Status $exe $Root $Account $statusOut
} catch {
}
$baseImages = 0
$baseClip = 0
$baseSscd = 0
if ($null -ne $baselineStatus) {
    $baseImages = To-IntOrZero $baselineStatus.images
    $baseClip = To-IntOrZero $baselineStatus.embeddings.active_clip_rows
    $baseSscd = To-IntOrZero $baselineStatus.embeddings.active_sscd_rows
}

$argsList = @(
    "image-index", "build",
    "--root", $Root,
    "--account", $Account,
    "--pipeline", "full",
    "--stage", $Stage,
    "--manifest-mode", $ManifestMode,
    "--ep", $Ep,
    "--limit", "$Limit",
    "--clip-batch-size", "$BatchSize",
    "--out", $Out
)
if ($ManifestWorkers -gt 0) {
    $argsList += @("--manifest-workers", "$ManifestWorkers")
}
foreach ($asset in $AssetRoot) {
    $argsList += @("--asset-root", $asset)
}
if ($Force) {
    $argsList += "--force"
}

$argLine = ($argsList | ForEach-Object { Quote-Argument $_ }) -join " "
$proc = Start-Process -FilePath $exe `
    -ArgumentList $argLine `
    -WorkingDirectory $repoPath `
    -RedirectStandardOutput $stdout `
    -RedirectStandardError $stderr `
    -PassThru

$started = Get-Date
$samples = New-Object System.Collections.Generic.List[object]
Write-Host "Started qq_analyzer_rs.exe PID=$($proc.Id) stage=$Stage ep=$Ep account=$Account manifest_mode=$ManifestMode manifest_workers=$ManifestWorkers batch=$BatchSize"

$lastImages = $baseImages
$lastClip = $baseClip
$lastSscd = $baseSscd
while (-not $proc.HasExited) {
    Start-Sleep -Seconds $PollSeconds
    $elapsed = ((Get-Date) - $started).TotalSeconds
    $status = $null
    try {
        $status = Read-Status $exe $Root $Account $statusOut
    } catch {
        Write-Host "PROGRESS elapsed=$([math]::Round($elapsed,1))s status_error=$($_.Exception.Message)"
        continue
    }
    if ($null -eq $status) {
        continue
    }
    $images = [int]($status.images)
    $clip = [int]($status.embeddings.active_clip_rows)
    $sscd = [int]($status.embeddings.active_sscd_rows)
    $missingClip = [int]($status.embeddings.active_images_missing_clip)
    $missingSscd = [int]($status.embeddings.active_images_missing_sscd)
    $errors = [int]($status.errors)
    $newImages = $images - $baseImages
    $newClip = $clip - $baseClip
    $newSscd = $sscd - $baseSscd
    $imageRate = if ($elapsed -gt 0) { [math]::Round($newImages / $elapsed, 2) } else { 0 }
    $clipRate = if ($elapsed -gt 0) { [math]::Round($newClip / $elapsed, 2) } else { 0 }
    $sscdRate = if ($elapsed -gt 0) { [math]::Round($newSscd / $elapsed, 2) } else { 0 }
    $deltaImages = $images - $lastImages
    $deltaClip = $clip - $lastClip
    $deltaSscd = $sscd - $lastSscd
    $lastImages = $images
    $lastClip = $clip
    $lastSscd = $sscd
    $gpu = Sample-GpuEngines $proc.Id
    $sample = [pscustomobject]@{
        elapsed_seconds = [math]::Round($elapsed, 3)
        images = $images
        errors = $errors
        active_clip_rows = $clip
        active_sscd_rows = $sscd
        missing_clip = $missingClip
        missing_sscd = $missingSscd
        new_images = $newImages
        new_clip = $newClip
        new_sscd = $newSscd
        images_per_second = $imageRate
        clip_rows_per_second = $clipRate
        sscd_rows_per_second = $sscdRate
        delta_images = $deltaImages
        delta_clip = $deltaClip
        delta_sscd = $deltaSscd
        gpu_active_engines = $gpu.active_engines
        gpu_max_utilization = $gpu.max_utilization
        gpu_top_engine = $gpu.top_engine
        gpu_error = $gpu.error
    }
    $samples.Add($sample)
    Write-Host "PROGRESS elapsed=$([math]::Round($elapsed,1))s images=$images errors=$errors clip=$clip sscd=$sscd missing_clip=$missingClip missing_sscd=$missingSscd new_img=$newImages new_clip=$newClip new_sscd=$newSscd rate_img=$imageRate/s rate_clip=$clipRate/s rate_ssCD=$sscdRate/s delta_img=$deltaImages delta_clip=$deltaClip delta_sscd=$deltaSscd gpu_engines=$($gpu.active_engines) gpu_max=$($gpu.max_utilization)% gpu_engine=$($gpu.top_engine)"
}

$proc.WaitForExit()
$finished = Get-Date
$exitCode = To-IntOrZero $proc.ExitCode
$buildReport = $null
if (Test-Path $Out) {
    $buildReport = Get-Content -Raw -Path $Out | ConvertFrom-Json
}
$finalStatus = $null
try {
    $finalStatus = Read-Status $exe $Root $Account $statusOut
} catch {
}

$summary = [pscustomobject]@{
    pid = $proc.Id
    exit_code = $exitCode
    stage = $Stage
    ep = $Ep
    manifest_mode = $ManifestMode
    manifest_workers = $ManifestWorkers
    batch_size = $BatchSize
    account = $Account
    root = $Root
    started_at = $started.ToString("o")
    finished_at = $finished.ToString("o")
    elapsed_seconds = [math]::Round(($finished - $started).TotalSeconds, 3)
    build_report_path = $Out
    stdout_path = $stdout
    stderr_path = $stderr
    status_path = $statusOut
    baseline = Compact-Status $baselineStatus
    samples = @($samples.ToArray())
    build_report = Compact-BuildReport $buildReport
    final_status = Compact-Status $finalStatus
}
$summary | ConvertTo-Json -Depth 10 | Set-Content -Encoding UTF8 -Path $summaryOut

Write-Host "Finished exit_code=$exitCode elapsed=$($summary.elapsed_seconds)s"
Write-Host "BuildReport=$Out"
Write-Host "ProgressReport=$summaryOut"
Write-Host "Stdout=$stdout"
Write-Host "Stderr=$stderr"

exit $exitCode
