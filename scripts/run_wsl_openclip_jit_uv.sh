#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
repo_dir="$(cd -- "$script_dir/.." && pwd)"
uv_cache="${UV_CACHE_DIR:-$(uv cache dir)}"
archive_dir="$uv_cache/archive-v0"

if [[ ! -d "$archive_dir" ]]; then
  echo "uv archive cache not found: $archive_dir" >&2
  exit 1
fi

find_module_dir() {
  local module="$1"
  while IFS= read -r dir; do
    if [[ -d "$dir/$module" || -f "$dir/$module.py" ]]; then
      printf '%s\n' "$dir"
      return 0
    fi
  done < <(find "$archive_dir" -mindepth 1 -maxdepth 1 -type d)
  return 1
}

pythonpath_parts=()
for module in \
  torch triton torchvision open_clip timm huggingface_hub safetensors ftfy \
  filelock wcwidth regex yaml requests packaging tqdm jinja2 markupsafe
do
  if module_dir="$(find_module_dir "$module" | head -n 1)" && [[ -n "${module_dir:-}" ]]; then
    pythonpath_parts+=("$module_dir")
  else
    echo "warning: module not found in uv cache archive: $module" >&2
  fi
done

ld_parts=()
while IFS= read -r dir; do
  ld_parts+=("$dir")
done < <(find "$archive_dir" -path '*/nvidia/*/lib' -type d)
if torch_dir="$(find_module_dir torch | head -n 1)" && [[ -n "${torch_dir:-}" ]]; then
  ld_parts+=("$torch_dir/torch/lib")
fi
while IFS= read -r dir; do
  ld_parts+=("$dir")
done < <(find "$archive_dir" -path '*/cusparselt/lib' -type d)

join_by_colon() {
  local IFS=:
  printf '%s' "$*"
}

export PYTHONPATH="$(join_by_colon "${pythonpath_parts[@]}")${PYTHONPATH:+:$PYTHONPATH}"
export LD_LIBRARY_PATH="$(join_by_colon "${ld_parts[@]}")${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
export TORCHINDUCTOR_CACHE_DIR="${TORCHINDUCTOR_CACHE_DIR:-$repo_dir/output/perf/torch-inductor-cache}"
export TRITON_CACHE_DIR="${TRITON_CACHE_DIR:-$repo_dir/output/perf/torch-triton-cache}"
export XDG_CACHE_HOME="${XDG_CACHE_HOME:-$repo_dir/output/perf/torch-cache}"

mkdir -p "$TORCHINDUCTOR_CACHE_DIR" "$TRITON_CACHE_DIR" "$XDG_CACHE_HOME"
exec uv run --active python "$repo_dir/scripts/bench_openclip_jit.py" "$@"
