#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'USAGE'
Usage:
  scripts/profile_wsl_cuda_image_index.sh --manifest <manifest.sqlite> [options]

Options:
  --mode <normal|nsys|ncu|all>       Default: normal
  --manifest <manifest.sqlite>       Source manifest to copy before profiling
  --out-dir <dir>                    Default: qq-analyzer/output/perf/wsl-cuda-<timestamp>
  --work-dir <dir>                   Default: /tmp/qq-analyzer-wsl-cuda-<timestamp>-work
  --root <workspace>                 Default: parent of qq-analyzer
  --account <name>                   Default: _wsl_cuda_profile
  --limit <n>                        Default: 2048
  --batch-size <n|auto>              Default: auto, use CLI default
  --model-dir <dir>                  Default: /tmp fp16 input model, then fp16 fp32-I/O model
  --sscd-model <onnx>                Default: /tmp fp16 input SSCD, then fp16 fp32-I/O SSCD
  --clip-model-key <name>            Default: inferred from selected CLIP model variant
  --sscd-model-key <name>            Default: inferred from selected SSCD model variant
  --embedding-kind <both|clip|sscd>  Default: both
  --ep <cuda|tensorrt|cpu|auto>      Default: cuda
  --cuda-graph                       Set QQ_ANALYZER_CUDA_GRAPH=1 for CUDA EP
  --no-cuda-graph                    Set QQ_ANALYZER_CUDA_GRAPH=0 for CUDA EP
  --renice-pid <pid>                 Temporarily renice an interfering process, then restore
  --renice-value <nice>              Nice value for --renice-pid. Default: 19
  --trt-cache-dir <dir>              Default: <out-dir>/tensorrt-cache when --ep tensorrt
  --trt-fp16                         Set QQ_ANALYZER_TENSORRT_FP16=1 when --ep tensorrt (default)
  --no-trt-fp16                      Do not set QQ_ANALYZER_TENSORRT_FP16
  --keep-embeddings                  Do not delete image_embeddings from copied manifest
  --ncu-launch-skip <n>              Default: 100
  --ncu-launch-count <n>             Default: 20

The script runs the release WSL binary, writes report.json and GPU dmon samples,
and for nsys/ncu modes writes profiler outputs into --out-dir.
USAGE
}

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
repo="$(cd -- "$script_dir/.." && pwd)"
workspace="$(cd -- "$repo/.." && pwd)"

mode="normal"
manifest_src=""
out_dir=""
work_dir=""
root="$workspace"
account="_wsl_cuda_profile"
limit="2048"
batch_size="auto"
model_dir=""
sscd_model=""
clip_model_key=""
sscd_model_key=""
embedding_kind="both"
ep="cuda"
cuda_graph=""
renice_pid=""
renice_value="19"
renice_original_value=""
trt_cache_dir=""
trt_fp16=1
keep_embeddings=0
ncu_launch_skip="100"
ncu_launch_count="20"

while [[ $# -gt 0 ]]; do
  case "$1" in
    --help|-h) usage; exit 0 ;;
    --mode) mode="${2:?}"; shift 2 ;;
    --manifest|--sqlite-path) manifest_src="${2:?}"; shift 2 ;;
    --out-dir) out_dir="${2:?}"; shift 2 ;;
    --work-dir|--stage-dir) work_dir="${2:?}"; shift 2 ;;
    --root) root="${2:?}"; shift 2 ;;
    --account) account="${2:?}"; shift 2 ;;
    --limit) limit="${2:?}"; shift 2 ;;
    --batch-size|--clip-batch-size) batch_size="${2:?}"; shift 2 ;;
    --model-dir) model_dir="${2:?}"; shift 2 ;;
    --sscd-model|--sscd-model-dir) sscd_model="${2:?}"; shift 2 ;;
    --clip-model-key) clip_model_key="${2:?}"; shift 2 ;;
    --sscd-model-key) sscd_model_key="${2:?}"; shift 2 ;;
    --embedding-kind) embedding_kind="${2:?}"; shift 2 ;;
    --ep|--execution-provider) ep="${2:?}"; shift 2 ;;
    --cuda-graph) cuda_graph=1; shift ;;
    --no-cuda-graph) cuda_graph=0; shift ;;
    --renice-pid) renice_pid="${2:?}"; shift 2 ;;
    --renice-value|--renice-nice) renice_value="${2:?}"; shift 2 ;;
    --trt-cache-dir|--tensorrt-cache-dir) trt_cache_dir="${2:?}"; shift 2 ;;
    --trt-fp16|--tensorrt-fp16) trt_fp16=1; shift ;;
    --no-trt-fp16|--no-tensorrt-fp16) trt_fp16=0; shift ;;
    --keep-embeddings) keep_embeddings=1; shift ;;
    --ncu-launch-skip) ncu_launch_skip="${2:?}"; shift 2 ;;
    --ncu-launch-count) ncu_launch_count="${2:?}"; shift 2 ;;
    *) echo "unknown argument: $1" >&2; usage >&2; exit 2 ;;
  esac
done

case "$mode" in
  normal|nsys|ncu|all) ;;
  *) echo "unknown --mode: $mode" >&2; exit 2 ;;
esac

if [[ -z "$manifest_src" ]]; then
  echo "--manifest <manifest.sqlite> is required" >&2
  exit 2
fi

manifest_src="$(realpath "$manifest_src")"
if [[ ! -f "$manifest_src" ]]; then
  echo "manifest does not exist: $manifest_src" >&2
  exit 1
fi

exe="$repo/target/release/qq_analyzer_rs"
if [[ ! -x "$exe" ]]; then
  echo "missing release executable: $exe" >&2
  echo "build it with cargo build --manifest-path qq-analyzer/rust-msg3-parser/Cargo.toml --release --features image-index,image-index-clip-cuda,image-index-sscd-cuda --bin qq_analyzer_rs" >&2
  exit 1
fi

if [[ -z "$model_dir" ]]; then
  for candidate in \
    "$repo/output/_deps/models/mobileclip2-s2-fp16-iofp32-gelu" \
    "/tmp/qq-analyzer-profile-models-fp16/mobileclip2-s2-fp16-inputfp16" \
    "$repo/output/_deps/models/mobileclip2-s2-fp16-inputfp16" \
    "/tmp/qq-analyzer-profile-models-fp16/mobileclip2-s2-fp16-iofp32" \
    "$repo/output/_deps/models/mobileclip2-s2-fp16-iofp32"; do
    if [[ -f "$candidate/visual.onnx" ]]; then
      model_dir="$candidate"
      break
    fi
  done
fi
if [[ -z "$model_dir" ]]; then
  model_dir="$repo/output/_deps/models/mobileclip2-s2-fp16-iofp32"
fi
if [[ -z "$clip_model_key" ]]; then
  case "$(basename "$model_dir")" in
    mobileclip2-s2-fp16-iofp32-gelu) clip_model_key="mobileclip2-s2-fp16-iofp32-gelu" ;;
    mobileclip2-s2-fp16-inputfp16) clip_model_key="mobileclip2-s2-fp16-inputfp16" ;;
    mobileclip2-s2-fp16-iofp32) clip_model_key="mobileclip2-s2-fp16-iofp32" ;;
    *) clip_model_key="mobileclip2-s2" ;;
  esac
fi

if [[ -z "$sscd_model" ]]; then
  for candidate in \
    "/tmp/qq-analyzer-profile-models-fp16/sscd-fp16-inputfp16/sscd_disc_mixup_fp16_inputfp16.onnx" \
    "$repo/output/_deps/models/sscd-fp16-inputfp16/sscd_disc_mixup_fp16_inputfp16.onnx" \
    "/tmp/qq-analyzer-profile-models-fp16/sscd-fp16-iofp32/sscd_disc_mixup_fp16_iofp32.onnx" \
    "$repo/output/_deps/models/sscd-fp16-iofp32/sscd_disc_mixup_fp16_iofp32.onnx"; do
    if [[ -f "$candidate" ]]; then
      sscd_model="$candidate"
      break
    fi
  done
fi
if [[ -z "$sscd_model" ]]; then
  sscd_model="$repo/output/_deps/models/sscd-fp16-iofp32/sscd_disc_mixup_fp16_iofp32.onnx"
fi
if [[ -z "$sscd_model_key" ]]; then
  case "$(basename "$sscd_model")" in
    sscd_disc_mixup_fp16_inputfp16.onnx) sscd_model_key="sscd_disc_mixup_fp16_inputfp16" ;;
    sscd_disc_mixup_fp16_iofp32.onnx) sscd_model_key="sscd_disc_mixup_fp16_iofp32" ;;
    *) sscd_model_key="sscd_disc_mixup" ;;
  esac
fi

timestamp="$(date +%Y%m%d-%H%M%S)"
if [[ -z "$out_dir" ]]; then
  out_dir="$repo/output/perf/wsl-cuda-${mode}-${timestamp}"
fi
mkdir -p "$out_dir"

if [[ -z "$work_dir" ]]; then
  work_dir="/tmp/qq-analyzer-wsl-cuda-${mode}-${timestamp}-work"
fi
mkdir -p "$work_dir"

manifest="$work_dir/manifest.sqlite"
final_manifest="$out_dir/manifest.sqlite"
cp "$manifest_src" "$manifest"
if [[ "$keep_embeddings" -eq 0 ]]; then
  sqlite3 "$manifest" "delete from image_embeddings; vacuum;"
fi

cudnn9="$(
  python3 - <<'PY' 2>/dev/null || true
import pathlib, site
for base in site.getsitepackages() + [site.getusersitepackages()]:
    p = pathlib.Path(base)
    if not p.exists():
        continue
    for q in p.rglob("libcudnn.so.9"):
        print(q.parent)
        raise SystemExit
PY
)"

tensorrt_libs="$(
  python3 - <<'PY' 2>/dev/null || true
import pathlib, site
for base in site.getsitepackages() + [site.getusersitepackages()]:
    p = pathlib.Path(base)
    if not p.exists():
        continue
    candidates = [p / "tensorrt_libs" / "libnvinfer.so.10"]
    candidates.extend(p.rglob("libnvinfer.so.10"))
    for q in candidates:
        if q.exists():
            print(q.parent)
            raise SystemExit
PY
)"

ld_parts=()
if [[ -n "$tensorrt_libs" ]]; then
  ld_parts+=("$tensorrt_libs")
fi
if [[ -n "$cudnn9" ]]; then
  ld_parts+=("$cudnn9")
fi
ld_parts+=("/usr/local/cuda/lib64" "/usr/local/cuda/targets/x86_64-linux/lib")
if [[ -n "${LD_LIBRARY_PATH:-}" ]]; then
  ld_parts+=("$LD_LIBRARY_PATH")
fi
export LD_LIBRARY_PATH="$(IFS=:; echo "${ld_parts[*]}")"
export QQ_ANALYZER_EMBEDDING_KIND="$embedding_kind"
if [[ -n "$cuda_graph" ]]; then
  export QQ_ANALYZER_CUDA_GRAPH="$cuda_graph"
fi

renice_with_fallback() {
  local value="$1"
  local pid="$2"
  if renice "$value" -p "$pid"; then
    return 0
  fi
  if [[ -x /mnt/c/Windows/System32/wsl.exe ]]; then
    /mnt/c/Windows/System32/wsl.exe -u root -- renice "$value" -p "$pid"
  else
    return 1
  fi
}

restore_renice() {
  if [[ -n "$renice_pid" && -n "$renice_original_value" ]]; then
    renice_with_fallback "$renice_original_value" "$renice_pid" \
      > "$out_dir/renice_restore.log" 2>&1 || true
  fi
}

apply_requested_renice() {
  if [[ -z "$renice_pid" ]]; then
    return 0
  fi
  if ! [[ "$renice_pid" =~ ^[0-9]+$ ]]; then
    echo "--renice-pid must be a numeric pid: $renice_pid" >&2
    exit 2
  fi
  if ! [[ "$renice_value" =~ ^-?[0-9]+$ ]]; then
    echo "--renice-value must be an integer nice value: $renice_value" >&2
    exit 2
  fi
  renice_original_value="$(ps -p "$renice_pid" -o ni= | tr -d '[:space:]')"
  if [[ -z "$renice_original_value" ]]; then
    echo "--renice-pid does not exist or has no nice value: $renice_pid" >&2
    exit 1
  fi
  {
    echo "renice_pid=$renice_pid"
    echo "renice_original_value=$renice_original_value"
    echo "renice_requested_value=$renice_value"
    ps -p "$renice_pid" -o pid,user,ni,pri,stat,pcpu,pmem,comm,args || true
  } > "$out_dir/renice_before.log"
  renice_with_fallback "$renice_value" "$renice_pid" > "$out_dir/renice_apply.log" 2>&1
  trap restore_renice EXIT
  ps -p "$renice_pid" -o pid,user,ni,pri,stat,pcpu,pmem,comm,args \
    > "$out_dir/renice_after_apply.log" 2>&1 || true
}

apply_requested_renice

ep_lower="${ep,,}"
if [[ "$ep_lower" == "tensorrt" || "$ep_lower" == "trt" ]]; then
  if [[ -z "$trt_cache_dir" ]]; then
    trt_cache_dir="$out_dir/tensorrt-cache"
  fi
  mkdir -p "$trt_cache_dir"
  export QQ_ANALYZER_TENSORRT_ENGINE_CACHE="${QQ_ANALYZER_TENSORRT_ENGINE_CACHE:-1}"
  export QQ_ANALYZER_TENSORRT_ENGINE_CACHE_PATH="${QQ_ANALYZER_TENSORRT_ENGINE_CACHE_PATH:-$trt_cache_dir}"
  if [[ "$trt_fp16" -eq 1 ]]; then
    export QQ_ANALYZER_TENSORRT_FP16="${QQ_ANALYZER_TENSORRT_FP16:-1}"
  fi
fi

cmd=(
  "$exe" image-index build
  --root "$root"
  --account "$account"
  --sqlite-path "$manifest"
  --stage embeddings
  --limit "$limit"
  --model-dir "$model_dir"
  --sscd-model-dir "$sscd_model"
  --clip-model "$clip_model_key"
  --sscd-model "$sscd_model_key"
  --ep "$ep"
  --out "$out_dir/report.json"
)
if [[ -n "$batch_size" && "$batch_size" != "auto" && "$batch_size" != "0" ]]; then
  cmd+=(--clip-batch-size "$batch_size")
fi

capture_env_snapshot() {
  local label="$1"
  local path="$out_dir/env_${label}.txt"
  {
    echo "timestamp=$(date -Is)"
    echo "label=$label"
    echo "hostname=$(hostname 2>/dev/null || true)"
    echo "kernel=$(uname -a 2>/dev/null || true)"
    echo "nproc=$(nproc 2>/dev/null || true)"
    echo "loadavg=$(cat /proc/loadavg 2>/dev/null || true)"
    echo "uptime=$(uptime 2>/dev/null || true)"
    echo "cpu_model=$(awk -F': ' '/model name/ {print $2; exit}' /proc/cpuinfo 2>/dev/null || true)"
    echo "memory_mb="
    free -m 2>/dev/null || true
    echo "top_cpu_processes="
    ps -eo pid,ppid,stat,pcpu,pmem,comm,args --sort=-pcpu 2>/dev/null | head -15 || true
    echo "gpu_query="
    nvidia-smi --query-gpu=name,driver_version,pstate,utilization.gpu,utilization.memory,memory.used,memory.total,power.draw \
      --format=csv,noheader,nounits 2>/dev/null || true
    echo "relevant_env="
    env | sort | grep -E '^(QQ_ANALYZER_|RAYON_|ORT_|CUDA_|LD_LIBRARY_PATH=)' || true
  } > "$path"
}

write_env_summary() {
  local summary_json="$out_dir/env_summary.json"
  python3 - "$out_dir" "$summary_json" <<'PY' || true
import json
import re
import sys
from pathlib import Path

out_dir = Path(sys.argv[1])
out_path = Path(sys.argv[2])

def parse_snapshot(path: Path) -> dict:
    text = path.read_text(errors="ignore") if path.is_file() else ""
    loadavg = None
    match = re.search(r"^loadavg=([^\n]+)", text, re.MULTILINE)
    if match:
        parts = match.group(1).split()
        if len(parts) >= 3:
            try:
                loadavg = [float(parts[0]), float(parts[1]), float(parts[2])]
            except ValueError:
                loadavg = None
    top_processes = []
    in_top = False
    for line in text.splitlines():
        if line == "top_cpu_processes=":
            in_top = True
            continue
        if in_top and line == "gpu_query=":
            break
        if not in_top or not line.strip() or line.lstrip().startswith("PID "):
            continue
        parts = line.split(None, 6)
        if len(parts) >= 7:
            try:
                cpu = float(parts[3])
                mem = float(parts[4])
            except ValueError:
                continue
            top_processes.append({
                "pid": int(parts[0]),
                "ppid": int(parts[1]),
                "stat": parts[2],
                "cpu_pct": cpu,
                "mem_pct": mem,
                "comm": parts[5],
                "args": parts[6],
            })
    gpu = None
    match = re.search(r"^gpu_query=\n([^\n]+)", text, re.MULTILINE)
    if match:
        values = [part.strip() for part in match.group(1).split(",")]
        if len(values) >= 8:
            gpu = {
                "name": values[0],
                "driver_version": values[1],
                "pstate": values[2],
                "util_gpu_pct": values[3],
                "util_mem_pct": values[4],
                "memory_used_mb": values[5],
                "memory_total_mb": values[6],
                "power_draw_w": values[7],
            }
    timestamp = None
    match = re.search(r"^timestamp=([^\n]+)", text, re.MULTILINE)
    if match:
        timestamp = match.group(1)
    return {
        "path": str(path),
        "timestamp": timestamp,
        "loadavg": loadavg,
        "top_cpu_processes": top_processes[:10],
        "gpu": gpu,
    }

snapshots = {}
for path in sorted(out_dir.glob("env_*.txt")):
    label = path.stem.removeprefix("env_")
    snapshots[label] = parse_snapshot(path)

out_path.write_text(json.dumps({"snapshots": snapshots}, indent=2, sort_keys=True) + "\n")
PY
}

run_with_dmon() {
  local label="$1"
  local stdout="$2"
  local stderr="$3"
  shift 3
  capture_env_snapshot "${label}_before"
  if nvidia-smi dmon -s pucvmet --gpm-metrics 2,3,7,9,10,12,13,20,21 --gpm-options d -d 1 -c 1 -o DT \
      > "$out_dir/gpu_dmon_probe.log" 2> "$out_dir/gpu_dmon_probe.err"; then
    nvidia-smi dmon -s pucvmet --gpm-metrics 2,3,7,9,10,12,13,20,21 --gpm-options d -d 1 -o DT \
      > "$out_dir/gpu_dmon.log" 2> "$out_dir/gpu_dmon.err" &
  else
    nvidia-smi dmon -s pucvmet -d 1 -o DT > "$out_dir/gpu_dmon.log" 2> "$out_dir/gpu_dmon.err" &
  fi
  local dmon_pid=$!
  set +e
  "$@" > "$stdout" 2> "$stderr"
  local status=$?
  set -e
  kill "$dmon_pid" 2>/dev/null || true
  wait "$dmon_pid" 2>/dev/null || true
  capture_env_snapshot "${label}_after"
  return "$status"
}

run_normal() {
  run_with_dmon normal "$out_dir/stdout.log" "$out_dir/stderr.log" "${cmd[@]}"
}

run_nsys() {
  export QQ_ANALYZER_CLIP_ORT_PROFILE="$out_dir/clip_ort_profile.json"
  export QQ_ANALYZER_SSCD_ORT_PROFILE="$out_dir/sscd_ort_profile.json"
  run_with_dmon nsys "$out_dir/nsys_stdout.log" "$out_dir/nsys_stderr.log" \
    nsys profile --force-overwrite true \
      -o "$out_dir/nsys_image_index" \
      --trace=cuda,nvtx,osrt,cudnn,cublas \
      --sample=cpu \
      --cpuctxsw=process-tree \
      --capture-range=none \
      "${cmd[@]}"
  if [[ -f "$out_dir/nsys_image_index.nsys-rep" ]]; then
    nsys export --type sqlite --force-overwrite true \
      --output "$out_dir/nsys_image_index.sqlite" \
      "$out_dir/nsys_image_index.nsys-rep" \
      > "$out_dir/nsys_export_stdout.log" 2> "$out_dir/nsys_export_stderr.log" || true
    for report in cuda_api_sum cuda_gpu_kern_sum cuda_gpu_mem_time_sum osrt_sum; do
      nsys stats --force-overwrite true --report "$report" --format csv \
        --output "$out_dir/nsys_${report}" \
        "$out_dir/nsys_image_index.nsys-rep" \
        > "$out_dir/nsys_stats_${report}_stdout.log" \
        2> "$out_dir/nsys_stats_${report}_stderr.log" || true
    done
  fi
}

run_ncu() {
  export QQ_ANALYZER_CLIP_ORT_PROFILE="$out_dir/clip_ort_profile.json"
  export QQ_ANALYZER_SSCD_ORT_PROFILE="$out_dir/sscd_ort_profile.json"
  set +e
  ncu --target-processes all \
    --kernel-name-base demangled \
    --launch-skip "$ncu_launch_skip" \
    --launch-count "$ncu_launch_count" \
    --section SpeedOfLight \
    --csv \
    --log-file "$out_dir/ncu_stdout.csv" \
    --export "$out_dir/ncu_report" \
    "${cmd[@]}" \
    > "$out_dir/ncu_app_stdout.log" \
    2> "$out_dir/ncu_app_stderr.log"
  local status=$?
  set -e
  echo "$status" > "$out_dir/ncu_exit_code"
  return "$status"
}

write_gpu_summary() {
  local dmon_log="$out_dir/gpu_dmon.log"
  local summary_json="$out_dir/gpu_summary.json"
  if [[ ! -f "$dmon_log" ]]; then
    return 0
  fi
  python3 - "$dmon_log" "$summary_json" <<'PY' || true
import json
import statistics
import sys
from pathlib import Path

log_path = Path(sys.argv[1])
out_path = Path(sys.argv[2])
rows = []
for line in log_path.read_text(errors="ignore").splitlines():
    if not line.startswith(" "):
        continue
    parts = line.split()
    if len(parts) < 24:
        continue
    def parse_float(index):
        if index >= len(parts) or parts[index] == "-":
            return None
        return float(parts[index])
    try:
        row = {
            "pwr_w": parse_float(3),
            "gtemp_c": parse_float(4),
            "sm_pct": parse_float(6),
            "mem_pct": parse_float(7),
            "fb_mb": parse_float(16),
            "pcie_rx_mbs": parse_float(22),
            "pcie_tx_mbs": parse_float(23),
            "gpm_smutil_pct": parse_float(24),
            "gpm_smocc_pct": parse_float(25),
            "gpm_hmma_pct": parse_float(26),
            "gpm_imma_pct": parse_float(27),
            "gpm_dram_pct": parse_float(28),
            "gpm_fp32_pct": parse_float(29),
            "gpm_fp16_pct": parse_float(30),
            "gpm_pcitx_mibs": parse_float(31),
            "gpm_pcirx_mibs": parse_float(32),
        }
        rows.append(row)
    except ValueError:
        continue

def avg(name: str) -> float:
    values = [row[name] for row in rows if row.get(name) is not None]
    return sum(values) / len(values) if values else 0.0

def p95(name: str) -> float:
    values = sorted(row[name] for row in rows if row.get(name) is not None)
    if not values:
        return 0.0
    index = min(len(values) - 1, int(round((len(values) - 1) * 0.95)))
    return values[index]

def maxv(name: str) -> float:
    return max((row[name] for row in rows if row.get(name) is not None), default=0.0)

def countv(name: str) -> int:
    return sum(1 for row in rows if row.get(name) is not None)

summary = {
    "samples": len(rows),
    "gpm_samples": max(
        countv("gpm_smutil_pct"),
        countv("gpm_hmma_pct"),
        countv("gpm_fp32_pct"),
        countv("gpm_fp16_pct"),
    ),
    "avg_sm_pct": avg("sm_pct"),
    "p95_sm_pct": p95("sm_pct"),
    "max_sm_pct": maxv("sm_pct"),
    "avg_mem_pct": avg("mem_pct"),
    "max_mem_pct": maxv("mem_pct"),
    "avg_fb_mb": avg("fb_mb"),
    "max_fb_mb": maxv("fb_mb"),
    "avg_power_w": avg("pwr_w"),
    "max_power_w": maxv("pwr_w"),
    "avg_gpu_temp_c": avg("gtemp_c"),
    "max_gpu_temp_c": maxv("gtemp_c"),
    "avg_pcie_rx_mbs": avg("pcie_rx_mbs"),
    "max_pcie_rx_mbs": maxv("pcie_rx_mbs"),
    "avg_pcie_tx_mbs": avg("pcie_tx_mbs"),
    "max_pcie_tx_mbs": maxv("pcie_tx_mbs"),
    "avg_gpm_smutil_pct": avg("gpm_smutil_pct"),
    "max_gpm_smutil_pct": maxv("gpm_smutil_pct"),
    "avg_gpm_smocc_pct": avg("gpm_smocc_pct"),
    "max_gpm_smocc_pct": maxv("gpm_smocc_pct"),
    "avg_gpm_hmma_pct": avg("gpm_hmma_pct"),
    "max_gpm_hmma_pct": maxv("gpm_hmma_pct"),
    "avg_gpm_imma_pct": avg("gpm_imma_pct"),
    "max_gpm_imma_pct": maxv("gpm_imma_pct"),
    "avg_gpm_dram_pct": avg("gpm_dram_pct"),
    "max_gpm_dram_pct": maxv("gpm_dram_pct"),
    "avg_gpm_fp32_pct": avg("gpm_fp32_pct"),
    "max_gpm_fp32_pct": maxv("gpm_fp32_pct"),
    "avg_gpm_fp16_pct": avg("gpm_fp16_pct"),
    "max_gpm_fp16_pct": maxv("gpm_fp16_pct"),
    "avg_gpm_pcitx_mibs": avg("gpm_pcitx_mibs"),
    "max_gpm_pcitx_mibs": maxv("gpm_pcitx_mibs"),
    "avg_gpm_pcirx_mibs": avg("gpm_pcirx_mibs"),
    "max_gpm_pcirx_mibs": maxv("gpm_pcirx_mibs"),
}
out_path.write_text(json.dumps(summary, indent=2, sort_keys=True) + "\n")
PY
}

write_perf_summary() {
  local report_json="$out_dir/report.json"
  local gpu_json="$out_dir/gpu_summary.json"
  local perf_json="$out_dir/perf_summary.json"
  if [[ ! -f "$report_json" ]]; then
    return 0
  fi
  python3 - "$report_json" "$gpu_json" "$perf_json" <<'PY' || true
import json
import sys
from pathlib import Path

report_path = Path(sys.argv[1])
gpu_path = Path(sys.argv[2])
out_path = Path(sys.argv[3])

report = json.loads(report_path.read_text())
gpu = json.loads(gpu_path.read_text()) if gpu_path.is_file() else {}
profile = report.get("embedding_profile") or {}

elapsed_ms = float(report.get("elapsed_ms") or 0.0)
embedded = int(report.get("embedded_files") or 0)
errors = int(report.get("error_files") or 0)

def value(name: str) -> float:
    return float(profile.get(name) or 0.0)

def pct(ms: float) -> float:
    return (ms / elapsed_ms * 100.0) if elapsed_ms > 0 else 0.0

clip_run_ms = value("clip_run_ms")
sscd_run_ms = value("sscd_run_ms")
clip_preprocess_ms = value("clip_preprocess_ms")
sscd_preprocess_ms = value("sscd_preprocess_ms")
sqlite_ms = value("sqlite_ms")
prepare_ms = value("prepare_ms")
pending_query_ms = value("pending_query_ms")
flush_ms = value("flush_ms")
avg_sm = float(gpu.get("avg_sm_pct") or 0.0)
max_sm = float(gpu.get("max_sm_pct") or 0.0)

if errors > 0 and embedded == 0:
    bottleneck = "errors"
elif pct(sqlite_ms) >= 20.0 or pct(pending_query_ms) >= 20.0:
    bottleneck = "sqlite_or_manifest_query"
elif avg_sm >= 70.0:
    bottleneck = "gpu_compute"
elif avg_sm < 35.0 and (pct(clip_preprocess_ms) + pct(sscd_preprocess_ms)) >= 25.0:
    bottleneck = "cpu_preprocess_or_input_pipeline"
elif max_sm >= 80.0 and avg_sm < 45.0:
    bottleneck = "bursty_gpu_with_cpu_gaps"
else:
    bottleneck = "mixed"

summary = {
    "elapsed_ms": elapsed_ms,
    "embedded_files": embedded,
    "error_files": errors,
    "images_per_sec": (embedded / (elapsed_ms / 1000.0)) if elapsed_ms > 0 else 0.0,
    "bottleneck_hint": bottleneck,
    "wall_time_pct": {
        "clip_run": pct(clip_run_ms),
        "sscd_run": pct(sscd_run_ms),
        "clip_preprocess": pct(clip_preprocess_ms),
        "sscd_preprocess": pct(sscd_preprocess_ms),
        "sqlite": pct(sqlite_ms),
        "prepare": pct(prepare_ms),
        "pending_query": pct(pending_query_ms),
        "flush": pct(flush_ms),
    },
    "ms": {
        "clip_run": clip_run_ms,
        "sscd_run": sscd_run_ms,
        "clip_preprocess": clip_preprocess_ms,
        "sscd_preprocess": sscd_preprocess_ms,
        "sqlite": sqlite_ms,
        "prepare": prepare_ms,
        "pending_query": pending_query_ms,
        "flush": flush_ms,
    },
    "gpu": gpu,
}
out_path.write_text(json.dumps(summary, indent=2, sort_keys=True) + "\n")
PY
}

case "$mode" in
  normal)
    run_normal
    ;;
  nsys)
    run_nsys
    ;;
  ncu)
    run_ncu || true
    ;;
  all)
    run_normal
    cp "$manifest_src" "$manifest"
    [[ "$keep_embeddings" -eq 1 ]] || sqlite3 "$manifest" "delete from image_embeddings; vacuum;"
    run_nsys
    cp "$manifest_src" "$manifest"
    [[ "$keep_embeddings" -eq 1 ]] || sqlite3 "$manifest" "delete from image_embeddings; vacuum;"
    run_ncu || true
    ;;
esac

if [[ "$manifest" != "$final_manifest" ]]; then
  cp "$manifest" "$final_manifest"
fi
write_gpu_summary
write_perf_summary
write_env_summary

{
  echo "out_dir=$out_dir"
  echo "work_dir=$work_dir"
  echo "mode=$mode"
  echo "manifest=$final_manifest"
  echo "work_manifest=$manifest"
  echo "model_dir=$model_dir"
  echo "sscd_model=$sscd_model"
  echo "clip_model_key=$clip_model_key"
  echo "sscd_model_key=$sscd_model_key"
  echo "embedding_kind=$embedding_kind"
  if [[ -n "$cuda_graph" ]]; then
    echo "cuda_graph=$cuda_graph"
  fi
  if [[ -n "$renice_pid" ]]; then
    echo "renice_pid=$renice_pid"
    echo "renice_value=$renice_value"
    echo "renice_original_value=$renice_original_value"
  fi
  echo "ld_library_path=$LD_LIBRARY_PATH"
  if [[ -n "$tensorrt_libs" ]]; then
    echo "tensorrt_libs=$tensorrt_libs"
  fi
  if [[ -n "$trt_cache_dir" ]]; then
    echo "tensorrt_cache_dir=$trt_cache_dir"
  fi
  if [[ -f "$out_dir/report.json" ]] && command -v jq >/dev/null 2>&1; then
    jq -r '"elapsed_ms=\(.elapsed_ms) embedded_files=\(.embedded_files) error_files=\(.error_files) clip_run_ms=\(.embedding_profile.clip_run_ms // 0) sscd_run_ms=\(.embedding_profile.sscd_run_ms // 0)"' "$out_dir/report.json"
  fi
  if [[ -f "$out_dir/gpu_summary.json" ]] && command -v jq >/dev/null 2>&1; then
    jq -r '"gpu_samples=\(.samples) avg_sm_pct=\(.avg_sm_pct|round) max_sm_pct=\(.max_sm_pct|round) avg_fb_mb=\(.avg_fb_mb|round) max_fb_mb=\(.max_fb_mb|round) avg_power_w=\(.avg_power_w|round) max_power_w=\(.max_power_w|round)"' "$out_dir/gpu_summary.json"
    jq -r '"gpm_samples=\(.gpm_samples) gpm_avg_smutil_pct=\(.avg_gpm_smutil_pct|round) gpm_max_smutil_pct=\(.max_gpm_smutil_pct|round) gpm_avg_smocc_pct=\(.avg_gpm_smocc_pct|round) gpm_avg_hmma_pct=\(.avg_gpm_hmma_pct|round) gpm_avg_fp32_pct=\(.avg_gpm_fp32_pct|round) gpm_avg_fp16_pct=\(.avg_gpm_fp16_pct|round) gpm_avg_dram_pct=\(.avg_gpm_dram_pct|round)"' "$out_dir/gpu_summary.json"
  fi
  if [[ -f "$out_dir/perf_summary.json" ]] && command -v jq >/dev/null 2>&1; then
    jq -r '"perf_images_per_sec=\(.images_per_sec|floor) bottleneck_hint=\(.bottleneck_hint) clip_run_pct=\(.wall_time_pct.clip_run|round) sscd_run_pct=\(.wall_time_pct.sscd_run|round) clip_preprocess_pct=\(.wall_time_pct.clip_preprocess|round) sscd_preprocess_pct=\(.wall_time_pct.sscd_preprocess|round)"' "$out_dir/perf_summary.json"
  fi
  if [[ -f "$out_dir/env_summary.json" ]] && command -v jq >/dev/null 2>&1; then
    jq -r '"env_before_loadavg=\(.snapshots.normal_before.loadavg // []) env_after_loadavg=\(.snapshots.normal_after.loadavg // [])"' "$out_dir/env_summary.json"
    jq -r '"env_before_top_cpu=\((.snapshots.normal_before.top_cpu_processes[0]? // {}).comm // "n/a"):\((.snapshots.normal_before.top_cpu_processes[0]? // {}).cpu_pct // 0) env_after_top_cpu=\((.snapshots.normal_after.top_cpu_processes[0]? // {}).comm // "n/a"):\((.snapshots.normal_after.top_cpu_processes[0]? // {}).cpu_pct // 0)"' "$out_dir/env_summary.json"
  fi
} | tee "$out_dir/summary.txt"
