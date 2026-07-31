param(
    [string]$Manifest = "",

    [string]$Root = "",
    [string]$Account = "_windows_cuda_profile",
    [ValidateSet("manifest", "embeddings", "all")]
    [string]$Stage = "embeddings",
    [int]$Limit = 2048,
    [string]$BatchSize = "128",
    [int]$DecodeWorkers = 0,
    [ValidateRange(0, 2)]
    [int]$PreparePrefetchDepth = 0,
    [string]$ModelDir = "",
    [string]$SscdModel = "",
    [string]$ClipModelKey = "",
    [string]$SscdModelKey = "",
    [string[]]$AssetRoot = @(),

    [ValidateSet("both", "clip", "sscd")]
    [string]$EmbeddingKind = "both",

    [ValidateSet("auto", "cuda", "tensorrt", "directml", "cpu")]
    [string]$Ep = "cuda",

    [string]$OutDir = "",
    [string]$SqlitePath = "",
    [string]$ReportPath = "",
    [string]$TrtCacheDir = "",
    [ValidateSet("", "none", "clip", "sscd", "all")]
    [string]$PrefetchEmbedInputs = "",

    [switch]$AsyncSqliteWriter,
    [switch]$NoAsyncSqliteWriter,
    [switch]$CudaGraph,
    [switch]$NoCudaGraph,
    [ValidateSet("", "0", "1")]
    [string]$CudaPreferNhwc = "",
    [switch]$ClipIoBinding,
    [switch]$NoClipIoBinding,
    [switch]$ClipCudaMemcpy,
    [switch]$ClipCudaMemcpyAsync,
    [switch]$NoClipCudaMemcpy,
    [switch]$SscdIoBinding,
    [switch]$NoSscdIoBinding,
    [ValidateSet("", "0", "1")]
    [string]$ScaledJpegDecode = "",
    [int]$ScaledJpegMinEdge = 0,
    [ValidateSet("", "0", "1")]
    [string]$ClipScaledJpegDecode = "",
    [int]$ClipScaledJpegMinEdge = 0,
    [switch]$TrtFp16,
    [switch]$TrtInt8,
    [switch]$SkipManifestProbe,
    [switch]$PrintOnly
)

$ErrorActionPreference = "Stop"
$batchSizeExplicit = $PSBoundParameters.ContainsKey("BatchSize")

if ($Ep -eq "tensorrt" -and $EmbeddingKind -eq "both") {
    throw "TensorRT CLIP+SSCD in one process is disabled after correctness validation failed. Run -EmbeddingKind clip, then -EmbeddingKind sscd."
}

function Resolve-FullPath([string]$PathValue) {
    if ([string]::IsNullOrWhiteSpace($PathValue)) {
        return ""
    }
    return (Resolve-Path -LiteralPath $PathValue).Path
}

function Quote-CmdArg([string]$Value) {
    if ($null -eq $Value -or $Value.Length -eq 0) {
        return '""'
    }
    if ($Value -notmatch '[\s"]') {
        return $Value
    }
    return '"' + $Value.Replace('"', '\"') + '"'
}

function Find-ExistingPath([string[]]$Candidates) {
    foreach ($candidate in $Candidates) {
        if (Test-Path -LiteralPath $candidate) {
            return (Resolve-Path -LiteralPath $candidate).Path
        }
    }
    return ""
}

function Get-AvailableAccounts([string]$BaseRoot) {
    if (-not (Test-Path -LiteralPath $BaseRoot)) {
        return @()
    }
    return @(Get-ChildItem -LiteralPath $BaseRoot -Directory |
        Where-Object { $_.Name -match '^\d+$' } |
        Sort-Object Name |
        ForEach-Object { $_.Name })
}

function Get-ImageAssetCount([string]$DatabasePath, [string]$ExePath, [string]$RootPath, [string]$AccountName, [string]$ScratchDir) {
    if (-not (Test-Path -LiteralPath $DatabasePath)) {
        return -1
    }
    $statusPath = Join-Path $ScratchDir "status-probe.json"
    Remove-Item -LiteralPath $statusPath -Force -ErrorAction SilentlyContinue
    & $ExePath image-index status --root $RootPath --account $AccountName --sqlite-path $DatabasePath --out $statusPath | Out-Null
    if ($LASTEXITCODE -ne 0 -or -not (Test-Path -LiteralPath $statusPath)) {
        return -1
    }
    try {
        $status = Get-Content -Raw -LiteralPath $statusPath | ConvertFrom-Json
    } catch {
        return -1
    }
    if ($null -eq $status) {
        return -1
    }
    return [int]$status.images
}

function Invoke-Analyzer([string]$ExePath, [string[]]$CommandArgs, [string]$Label) {
    Write-Host ""
    Write-Host "$Label arguments:"
    Write-Host (($CommandArgs | ForEach-Object { Quote-CmdArg $_ }) -join " ")
    if ($PrintOnly) {
        return
    }
    & $ExePath @CommandArgs
    if ($LASTEXITCODE -ne 0) {
        exit $LASTEXITCODE
    }
}

$scriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$repoPath = Resolve-FullPath (Join-Path $scriptDir "..")
$workspacePath = Resolve-FullPath (Join-Path $repoPath "..")
$exe = Join-Path $repoPath "target\release\qq_analyzer_rs.exe"
if (-not (Test-Path -LiteralPath $exe)) {
    throw "Missing release executable: $exe"
}

if (-not $Root) {
    $Root = $workspacePath
}
$Root = Resolve-FullPath $Root

if (-not $OutDir) {
    $stamp = Get-Date -Format "yyyyMMdd-HHmmss"
    $OutDir = Join-Path $repoPath "output\perf\windows-image-index-$stamp"
}
New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
$OutDir = Resolve-FullPath $OutDir

if (-not $SqlitePath) {
    $SqlitePath = Join-Path $OutDir "manifest.sqlite"
}
if (-not $ReportPath) {
    $ReportPath = Join-Path $OutDir "report.json"
}

$manifestPath = ""
if ($Manifest) {
    $manifestPath = Resolve-FullPath $Manifest
    if (-not $manifestPath) {
        throw "Manifest not found: $Manifest"
    }
}

if ($manifestPath -and $manifestPath -ne $SqlitePath) {
    Remove-Item -LiteralPath $SqlitePath -Force -ErrorAction SilentlyContinue
    Remove-Item -LiteralPath ($SqlitePath + "-wal") -Force -ErrorAction SilentlyContinue
    Remove-Item -LiteralPath ($SqlitePath + "-shm") -Force -ErrorAction SilentlyContinue
    Copy-Item -LiteralPath $manifestPath -Destination $SqlitePath -Force
}
if ((-not $manifestPath) -and (Test-Path -LiteralPath $SqlitePath)) {
    $manifestPath = Resolve-FullPath $SqlitePath
}

if (-not $ModelDir) {
    if ($Ep -eq "tensorrt") {
        $ModelDir = Find-ExistingPath @(
            (Join-Path $repoPath "output\_deps\models\mobileclip2-s2-fp16-inputfp16-gelu-static-b512"),
            (Join-Path $repoPath "output\_deps\models\mobileclip2-s2-fp16-iofp32-gelu-static-b512"),
            (Join-Path $repoPath "output\_deps\models\mobileclip2-s2-fp16-inputfp16-gelu-static-b256"),
            (Join-Path $repoPath "output\_deps\models\mobileclip2-s2-fp16-iofp32-gelu-static-b256"),
            (Join-Path $repoPath "output\_deps\models\mobileclip2-s2-fp16-inputfp16-gelu-static-b128"),
            (Join-Path $repoPath "output\_deps\models\mobileclip2-s2-fp16-iofp32-gelu-static-b128"),
            (Join-Path $repoPath "output\_deps\models\mobileclip2-s2-fp16-iofp32-gelu-static-b64"),
            (Join-Path $repoPath "output\_deps\models\mobileclip2-s2-fp16-iofp32-gelu-static-b32"),
            (Join-Path $repoPath "output\_deps\models\mobileclip2-s2-fp16-iofp32-gelu"),
            (Join-Path $repoPath "output\_deps\models\mobileclip2-s2-fp16-iofp32")
        )
    } else {
        $ModelDir = Find-ExistingPath @(
            (Join-Path $repoPath "output\_deps\models\mobileclip2-s2-fp16-iofp32-gelu"),
            (Join-Path $repoPath "output\_deps\models\mobileclip2-s2-fp16-inputfp16"),
            (Join-Path $repoPath "output\_deps\models\mobileclip2-s2-fp16-iofp32")
        )
    }
}
if (-not $ModelDir) {
    throw "No CLIP model directory found. Pass -ModelDir."
}

$modelName = [IO.Path]::GetFileName($ModelDir)
if ((-not $batchSizeExplicit) -and $modelName -match '-static-b(\d+)$') {
    $BatchSize = $Matches[1]
}
if ((-not $batchSizeExplicit) -and $EmbeddingKind -eq "sscd") {
    $BatchSize = "256"
}

if (-not $ClipModelKey) {
    if ($modelName -match '^mobileclip2-s2-fp16-(iofp32|inputfp16)-gelu-static-b\d+$') {
        $ClipModelKey = $modelName
    } else {
        switch -Exact ($modelName) {
        "mobileclip2-s2-fp16-iofp32-gelu" { $ClipModelKey = "mobileclip2-s2-fp16-iofp32-gelu" }
        "mobileclip2-s2-fp16-inputfp16-gelu" { $ClipModelKey = "mobileclip2-s2-fp16-inputfp16-gelu" }
        "mobileclip2-s2-fp16-inputfp16" { $ClipModelKey = "mobileclip2-s2-fp16-inputfp16" }
        "mobileclip2-s2-fp16-iofp32" { $ClipModelKey = "mobileclip2-s2-fp16-iofp32" }
        default { $ClipModelKey = "mobileclip2-s2" }
        }
    }
}

if (-not $SscdModel) {
    if ($Ep -eq "tensorrt" -and [int]$BatchSize -eq 256) {
        $SscdModel = Find-ExistingPath @(
            (Join-Path $repoPath "output\_deps\models\sscd-fp16-inputfp16-static-b256-frontreshape\sscd_disc_mixup_fp16_inputfp16_static_b256_frontreshape.onnx"),
            (Join-Path $repoPath "output\_deps\models\sscd-fp16-inputfp16\sscd_disc_mixup_fp16_inputfp16.onnx"),
            (Join-Path $repoPath "output\_deps\models\sscd-fp16-iofp32\sscd_disc_mixup_fp16_iofp32.onnx")
        )
    } elseif ($Ep -eq "tensorrt" -and [int]$BatchSize -le 128) {
        $SscdModel = Find-ExistingPath @(
            (Join-Path $repoPath "output\_deps\models\sscd-fp16-inputfp16-static-b128-frontreshape\sscd_disc_mixup_fp16_inputfp16_static_b128_frontreshape.onnx"),
            (Join-Path $repoPath "output\_deps\models\sscd-fp16-inputfp16\sscd_disc_mixup_fp16_inputfp16.onnx"),
            (Join-Path $repoPath "output\_deps\models\sscd-fp16-iofp32\sscd_disc_mixup_fp16_iofp32.onnx")
        )
    } else {
        $SscdModel = Find-ExistingPath @(
            (Join-Path $repoPath "output\_deps\models\sscd-fp16-inputfp16\sscd_disc_mixup_fp16_inputfp16.onnx"),
            (Join-Path $repoPath "output\_deps\models\sscd-fp16-iofp32\sscd_disc_mixup_fp16_iofp32.onnx")
        )
    }
}
if (-not $SscdModel) {
    throw "No SSCD model found. Pass -SscdModel."
}

if (-not $SscdModelKey) {
    switch -Exact ([IO.Path]::GetFileName($SscdModel)) {
        "sscd_disc_mixup_fp16_inputfp16.onnx" { $SscdModelKey = "sscd_disc_mixup_fp16_inputfp16" }
        "sscd_disc_mixup_fp16_iofp32.onnx" { $SscdModelKey = "sscd_disc_mixup_fp16_iofp32" }
        default { $SscdModelKey = "sscd_disc_mixup" }
    }
}

if ($Ep -eq "cuda" -or $Ep -eq "tensorrt") {
    $cudaRoot = Join-Path $repoPath "output\_deps\nvidia-cu12-win\nvidia"
    $cudaDllDirs = @(
        (Join-Path $cudaRoot "cublas\bin"),
        (Join-Path $cudaRoot "cuda_nvrtc\bin"),
        (Join-Path $cudaRoot "cudnn\bin"),
        "C:\Program Files\NVIDIA GPU Computing Toolkit\CUDA\v12.5\bin"
    ) | Where-Object { Test-Path -LiteralPath $_ }
    if ($cudaDllDirs.Count -gt 0) {
        $env:PATH = (($cudaDllDirs -join ";") + ";" + $env:PATH)
    }
}
if ($Ep -eq "tensorrt") {
    $trtDllDirs = @(
        (Join-Path $repoPath "output\_deps\tensorrt-cu12-py\tensorrt_libs"),
        (Join-Path $repoPath "output\_deps\tensorrt\lib"),
        (Join-Path $repoPath "output\_deps\tensorrt\bin"),
        "C:\Program Files\NVIDIA GPU Computing Toolkit\TensorRT\lib",
        "C:\Program Files\NVIDIA GPU Computing Toolkit\TensorRT\bin",
        "C:\Program Files\NVIDIA\TensorRT\lib",
        "C:\Program Files\NVIDIA\TensorRT\bin"
    ) | Where-Object { Test-Path -LiteralPath $_ }
    if ($trtDllDirs.Count -gt 0) {
        $env:PATH = (($trtDllDirs -join ";") + ";" + $env:PATH)
    }
}

$turboJpegDll = Find-ExistingPath @(
    (Join-Path $repoPath "output\_deps\libjpeg-turbo\vc-x64\bin\turbojpeg.dll"),
    "C:\libjpeg-turbo64\bin\turbojpeg.dll"
)
if ($turboJpegDll) {
    $env:QQ_ANALYZER_TURBOJPEG_DLL = $turboJpegDll
}
if ($ScaledJpegDecode) {
    $env:QQ_ANALYZER_SSCD_SCALED_JPEG_DECODE = $ScaledJpegDecode
}
if ($ScaledJpegMinEdge -gt 0) {
    $env:QQ_ANALYZER_SSCD_SCALED_JPEG_MIN_EDGE = "$ScaledJpegMinEdge"
}
if ($ClipScaledJpegDecode) {
    $env:QQ_ANALYZER_CLIP_SCALED_JPEG_DECODE = $ClipScaledJpegDecode
}
if ($ClipScaledJpegMinEdge -gt 0) {
    $env:QQ_ANALYZER_CLIP_SCALED_JPEG_MIN_EDGE = "$ClipScaledJpegMinEdge"
}
if ($PreparePrefetchDepth -gt 0) {
    $env:QQ_ANALYZER_PREPARE_PREFETCH_DEPTH = "$PreparePrefetchDepth"
}

$env:QQ_ANALYZER_EMBEDDING_KIND = $EmbeddingKind
if ($CudaGraph) {
    $env:QQ_ANALYZER_CUDA_GRAPH = "1"
}
if ($NoCudaGraph) {
    $env:QQ_ANALYZER_CUDA_GRAPH = "0"
}
if ($CudaPreferNhwc) {
    $env:QQ_ANALYZER_CUDA_PREFER_NHWC = $CudaPreferNhwc
}
$defaultClipIoBinding = (
    $Ep -eq "tensorrt" -and
    $EmbeddingKind -eq "clip" -and
    -not $NoClipIoBinding
)
if ($ClipIoBinding -or $defaultClipIoBinding) {
    $env:QQ_ANALYZER_CLIP_IO_BINDING = "1"
}
if ($NoClipIoBinding) {
    $env:QQ_ANALYZER_CLIP_IO_BINDING = "0"
}
$defaultClipCudaMemcpy = (
    $Ep -eq "tensorrt" -and
    $EmbeddingKind -eq "clip" -and
    -not $NoClipCudaMemcpy
)
if ($ClipCudaMemcpy -or $defaultClipCudaMemcpy) {
    $env:QQ_ANALYZER_CLIP_CUDA_MEMCPY = "1"
}
if ($ClipCudaMemcpyAsync) {
    $env:QQ_ANALYZER_CLIP_CUDA_MEMCPY_ASYNC = "1"
}
if ($NoClipCudaMemcpy) {
    $env:QQ_ANALYZER_CLIP_CUDA_MEMCPY = "0"
}
$sscdModelName = [IO.Path]::GetFileNameWithoutExtension($SscdModel)
$defaultSscdIoBinding = (
    $Ep -eq "tensorrt" -and
    $sscdModelName -match 'static[-_]b\d+' -and
    -not $NoSscdIoBinding
)
if ($SscdIoBinding -or $defaultSscdIoBinding) {
    $env:QQ_ANALYZER_SSCD_IO_BINDING = "1"
}
if ($NoSscdIoBinding) {
    $env:QQ_ANALYZER_SSCD_IO_BINDING = "0"
}
if ((-not $PrefetchEmbedInputs) -and $Ep -eq "tensorrt" -and $Stage -ne "manifest") {
    if ($EmbeddingKind -eq "sscd") {
        $PrefetchEmbedInputs = "sscd"
    } else {
        $PrefetchEmbedInputs = "clip"
    }
}
if ($PrefetchEmbedInputs) {
    if ($PrefetchEmbedInputs -eq "none") {
        $env:QQ_ANALYZER_PREFETCH_EMBED_INPUTS = "0"
    } else {
        $env:QQ_ANALYZER_PREFETCH_EMBED_INPUTS = $PrefetchEmbedInputs
    }
}
if ($AsyncSqliteWriter) {
    $env:QQ_ANALYZER_ASYNC_SQLITE_WRITER = "1"
}
if ($NoAsyncSqliteWriter) {
    $env:QQ_ANALYZER_ASYNC_SQLITE_WRITER = "0"
}
if ($Ep -eq "tensorrt") {
    if (-not $TrtCacheDir) {
        switch ($EmbeddingKind) {
            "clip" { $trtCacheKey = "clip-$modelName" }
            "sscd" { $trtCacheKey = "sscd-$sscdModelName" }
            default { $trtCacheKey = "both-$modelName-$sscdModelName" }
        }
        $trtCacheKey = $trtCacheKey -replace '[^A-Za-z0-9._-]', '_'
        $TrtCacheDir = Join-Path $repoPath "output\_deps\tensorrt-cache\$trtCacheKey"
    }
    New-Item -ItemType Directory -Force -Path $TrtCacheDir | Out-Null
    $env:QQ_ANALYZER_TENSORRT_ENGINE_CACHE = "1"
    $env:QQ_ANALYZER_TENSORRT_ENGINE_CACHE_PATH = (Resolve-FullPath $TrtCacheDir)
    $defaultTrtFp16 = ($modelName -match 'fp16')
    if ($TrtFp16 -or $defaultTrtFp16) {
        $env:QQ_ANALYZER_TENSORRT_FP16 = "1"
    }
    if ($TrtInt8) {
        $env:QQ_ANALYZER_TENSORRT_INT8 = "1"
    }
    $defaultTrtCudaGraph = (
        $EmbeddingKind -eq "sscd" -and
        $modelName -match '-static-b\d+$' -and
        -not $NoCudaGraph
    )
    if ($CudaGraph -or $defaultTrtCudaGraph) {
        $env:QQ_ANALYZER_TENSORRT_CUDA_GRAPH = "1"
    }
    if ($EmbeddingKind -eq "clip" -and -not $CudaGraph) {
        $env:QQ_ANALYZER_TENSORRT_CUDA_GRAPH = "0"
    }
    if ($NoCudaGraph) {
        $env:QQ_ANALYZER_TENSORRT_CUDA_GRAPH = "0"
    }
}

$availableAccounts = Get-AvailableAccounts $Root
$needsRootScan = ($Stage -ne "embeddings")
if ($Stage -eq "embeddings" -and -not $SkipManifestProbe) {
    $assetCount = Get-ImageAssetCount $SqlitePath $exe $Root $Account $OutDir
    if ($assetCount -le 0) {
        $needsRootScan = $true
    }
}
if ($needsRootScan -and $Account -eq "_windows_cuda_profile" -and $AssetRoot.Count -eq 0) {
    if ($availableAccounts.Count -eq 1) {
        $Account = $availableAccounts[0]
    } elseif ($availableAccounts.Count -gt 1) {
        throw "Account is still the placeholder value. Use -Account with one of: $($availableAccounts -join ', ')"
    }
}

$commonArgs = @(
    "image-index", "build",
    "--root", $Root,
    "--account", $Account,
    "--sqlite-path", $SqlitePath,
    "--limit", "$Limit"
)
if ($DecodeWorkers -gt 0) {
    $commonArgs += @("--manifest-workers", "$DecodeWorkers")
}
foreach ($asset in $AssetRoot) {
    $commonArgs += @("--asset-root", (Resolve-FullPath $asset))
}

$prepareManifest = $false
if ($Stage -eq "embeddings" -and -not $SkipManifestProbe) {
    $assetCount = Get-ImageAssetCount $SqlitePath $exe $Root $Account $OutDir
    if ($assetCount -le 0) {
        $prepareManifest = $true
    }
}

if ($prepareManifest) {
    Write-Host "Preparing manifest because image_assets is empty or missing."
    $manifestArgs = @(
        $commonArgs +
        @(
            "--stage", "manifest",
            "--out", (Join-Path $OutDir "manifest-report.json")
        )
    )
    Invoke-Analyzer $exe $manifestArgs "Manifest"
    if (-not $PrintOnly) {
        $preparedAssetCount = Get-ImageAssetCount $SqlitePath $exe $Root $Account $OutDir
        if ($preparedAssetCount -le 0) {
            throw "Prepared manifest still has no image_assets. Check -Account or pass -AssetRoot explicitly."
        }
    }
}

$argsList = @(
    $commonArgs +
    @(
        "--stage", $Stage,
        "--model-dir", $ModelDir,
        "--sscd-model-dir", $SscdModel,
        "--clip-model", $ClipModelKey,
        "--sscd-model", $SscdModelKey,
        "--ep", $Ep,
        "--out", $ReportPath
    )
)
if ($Stage -ne "manifest" -and $BatchSize -and $BatchSize -ne "auto" -and $BatchSize -ne "0") {
    $argsList += @("--clip-batch-size", $BatchSize)
}

Write-Host "Profiler target:"
Write-Host $exe
Write-Host ""
Write-Host "Environment overrides:"
Write-Host "QQ_ANALYZER_EMBEDDING_KIND=$($env:QQ_ANALYZER_EMBEDDING_KIND)"
if ($env:QQ_ANALYZER_TURBOJPEG_DLL) {
    Write-Host "QQ_ANALYZER_TURBOJPEG_DLL=$($env:QQ_ANALYZER_TURBOJPEG_DLL)"
}
if ($env:QQ_ANALYZER_SSCD_SCALED_JPEG_DECODE) {
    Write-Host "QQ_ANALYZER_SSCD_SCALED_JPEG_DECODE=$($env:QQ_ANALYZER_SSCD_SCALED_JPEG_DECODE)"
}
if ($env:QQ_ANALYZER_SSCD_SCALED_JPEG_MIN_EDGE) {
    Write-Host "QQ_ANALYZER_SSCD_SCALED_JPEG_MIN_EDGE=$($env:QQ_ANALYZER_SSCD_SCALED_JPEG_MIN_EDGE)"
}
if ($env:QQ_ANALYZER_CLIP_SCALED_JPEG_DECODE) {
    Write-Host "QQ_ANALYZER_CLIP_SCALED_JPEG_DECODE=$($env:QQ_ANALYZER_CLIP_SCALED_JPEG_DECODE)"
}
if ($env:QQ_ANALYZER_CLIP_SCALED_JPEG_MIN_EDGE) {
    Write-Host "QQ_ANALYZER_CLIP_SCALED_JPEG_MIN_EDGE=$($env:QQ_ANALYZER_CLIP_SCALED_JPEG_MIN_EDGE)"
}
if ($env:QQ_ANALYZER_PREPARE_PREFETCH_DEPTH) {
    Write-Host "QQ_ANALYZER_PREPARE_PREFETCH_DEPTH=$($env:QQ_ANALYZER_PREPARE_PREFETCH_DEPTH)"
}
if ($env:QQ_ANALYZER_CUDA_GRAPH) {
    Write-Host "QQ_ANALYZER_CUDA_GRAPH=$($env:QQ_ANALYZER_CUDA_GRAPH)"
}
if ($env:QQ_ANALYZER_CUDA_PREFER_NHWC) {
    Write-Host "QQ_ANALYZER_CUDA_PREFER_NHWC=$($env:QQ_ANALYZER_CUDA_PREFER_NHWC)"
}
if ($env:QQ_ANALYZER_CLIP_IO_BINDING) {
    Write-Host "QQ_ANALYZER_CLIP_IO_BINDING=$($env:QQ_ANALYZER_CLIP_IO_BINDING)"
}
if ($env:QQ_ANALYZER_CLIP_CUDA_MEMCPY) {
    Write-Host "QQ_ANALYZER_CLIP_CUDA_MEMCPY=$($env:QQ_ANALYZER_CLIP_CUDA_MEMCPY)"
}
if ($env:QQ_ANALYZER_CLIP_CUDA_MEMCPY_ASYNC) {
    Write-Host "QQ_ANALYZER_CLIP_CUDA_MEMCPY_ASYNC=$($env:QQ_ANALYZER_CLIP_CUDA_MEMCPY_ASYNC)"
}
if ($env:QQ_ANALYZER_SSCD_IO_BINDING) {
    Write-Host "QQ_ANALYZER_SSCD_IO_BINDING=$($env:QQ_ANALYZER_SSCD_IO_BINDING)"
}
if ($env:QQ_ANALYZER_PREFETCH_EMBED_INPUTS) {
    Write-Host "QQ_ANALYZER_PREFETCH_EMBED_INPUTS=$($env:QQ_ANALYZER_PREFETCH_EMBED_INPUTS)"
}
if ($env:QQ_ANALYZER_ASYNC_SQLITE_WRITER) {
    Write-Host "QQ_ANALYZER_ASYNC_SQLITE_WRITER=$($env:QQ_ANALYZER_ASYNC_SQLITE_WRITER)"
}
if ($env:QQ_ANALYZER_TENSORRT_ENGINE_CACHE) {
    Write-Host "QQ_ANALYZER_TENSORRT_ENGINE_CACHE=$($env:QQ_ANALYZER_TENSORRT_ENGINE_CACHE)"
    Write-Host "QQ_ANALYZER_TENSORRT_ENGINE_CACHE_PATH=$($env:QQ_ANALYZER_TENSORRT_ENGINE_CACHE_PATH)"
}
if ($env:QQ_ANALYZER_TENSORRT_FP16) {
    Write-Host "QQ_ANALYZER_TENSORRT_FP16=$($env:QQ_ANALYZER_TENSORRT_FP16)"
}
if ($env:QQ_ANALYZER_TENSORRT_INT8) {
    Write-Host "QQ_ANALYZER_TENSORRT_INT8=$($env:QQ_ANALYZER_TENSORRT_INT8)"
}
if ($env:QQ_ANALYZER_TENSORRT_CUDA_GRAPH) {
    Write-Host "QQ_ANALYZER_TENSORRT_CUDA_GRAPH=$($env:QQ_ANALYZER_TENSORRT_CUDA_GRAPH)"
}
Write-Host ""
Write-Host "Output dir: $OutDir"

if ($PrintOnly) {
    if ($prepareManifest) {
        Write-Host ""
        Write-Host "This run requires manifest preparation before embeddings."
    }
    Invoke-Analyzer $exe $argsList "Profiler"
    exit 0
}

Invoke-Analyzer $exe $argsList "Profiler"
exit 0
