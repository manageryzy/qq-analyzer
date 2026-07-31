from __future__ import annotations

import argparse
import ctypes
import json
import os
import sqlite3
import sys
import time
from pathlib import Path
from typing import Any

import numpy as np


CUDA_SUCCESS = 0
CUDA_MEMCPY_HOST_TO_DEVICE = 1


def load_tensorrt(repo: Path) -> Any:
    trt_py = repo / "output" / "_deps" / "tensorrt-cu12-py"
    if trt_py.is_dir():
        sys.path.insert(0, str(trt_py))
    import tensorrt as trt

    return trt


def tensor_shape(tensor: Any) -> list[int]:
    return [int(dim) for dim in tensor.shape]


def set_builder_flag_if_available(trt: Any, config: Any, flag_name: str) -> bool:
    try:
        flag = getattr(trt.BuilderFlag, flag_name)
    except AttributeError:
        return False
    config.set_flag(flag)
    return True


def set_memory_pool_mib_if_available(
    trt: Any, config: Any, pool_name: str, mib: int | None
) -> dict[str, Any] | None:
    if mib is None:
        return None
    try:
        pool = getattr(trt.MemoryPoolType, pool_name)
    except AttributeError:
        return {"pool": pool_name, "requested_mib": mib, "available": False}
    bytes_value = int(mib) * 1024 * 1024
    config.set_memory_pool_limit(pool, bytes_value)
    return {
        "pool": pool_name,
        "requested_mib": int(mib),
        "bytes": bytes_value,
        "available": True,
    }


def set_memory_pool_kib_if_available(
    trt: Any, config: Any, pool_name: str, kib: int | None
) -> dict[str, Any] | None:
    if kib is None:
        return None
    try:
        pool = getattr(trt.MemoryPoolType, pool_name)
    except AttributeError:
        return {"pool": pool_name, "requested_kib": kib, "available": False}
    bytes_value = int(kib) * 1024
    config.set_memory_pool_limit(pool, bytes_value)
    return {
        "pool": pool_name,
        "requested_kib": int(kib),
        "bytes": bytes_value,
        "available": True,
    }


def configure_tactic_sources(trt: Any, config: Any, disabled: list[str]) -> dict[str, Any] | None:
    if not disabled:
        return None
    disabled_upper = {name.upper() for name in disabled}
    available = [name for name in dir(trt.TacticSource) if name.isupper()]
    enabled = [name for name in available if name not in disabled_upper]
    missing = sorted(disabled_upper - set(available))
    mask = 0
    for name in enabled:
        mask |= 1 << int(getattr(trt.TacticSource, name))
    config.set_tactic_sources(mask)
    return {
        "available": available,
        "disabled": sorted(disabled_upper),
        "enabled": enabled,
        "missing_disabled": missing,
        "mask": mask,
    }


def set_profiling_verbosity(trt: Any, config: Any, requested: str) -> str:
    if requested == "default":
        return "default"
    enum_name = requested.upper()
    try:
        value = getattr(trt.ProfilingVerbosity, enum_name)
    except AttributeError as exc:
        raise RuntimeError(f"TensorRT does not expose ProfilingVerbosity.{enum_name}") from exc
    config.profiling_verbosity = value
    return str(value)


def force_layer_fp16(trt: Any, network: Any, names: list[str]) -> list[dict[str, Any]]:
    wanted = set(names)
    changed: list[dict[str, Any]] = []
    if not wanted:
        return changed
    for index in range(network.num_layers):
        layer = network.get_layer(index)
        if layer.name not in wanted:
            continue
        row: dict[str, Any] = {
            "index": index,
            "name": layer.name,
            "type": str(layer.type),
            "output_count": int(getattr(layer, "num_outputs", 0)),
        }
        layer.precision = trt.DataType.HALF
        row["precision"] = str(layer.precision)
        output_types = []
        for output_index in range(row["output_count"]):
            try:
                layer.set_output_type(output_index, trt.DataType.HALF)
                output_types.append(str(layer.get_output(output_index).dtype))
            except Exception as exc:
                output_types.append(f"{type(exc).__name__}: {exc}")
        row["output_types"] = output_types
        changed.append(row)
    missing = sorted(wanted - {row["name"] for row in changed})
    for name in missing:
        changed.append({"name": name, "missing": True})
    return changed


def resolve_cuda_runtime_path() -> Path | None:
    candidates: list[Path] = []
    for env_name, env_value in os.environ.items():
        if env_name.upper().startswith("CUDA_PATH") and env_value:
            candidates.append(Path(env_value) / "bin" / "cudart64_12.dll")
    for version in ("v12.5", "v12.4", "v12.3", "v12.2", "v12.1", "v12.0"):
        candidates.append(
            Path("C:/Program Files/NVIDIA GPU Computing Toolkit/CUDA")
            / version
            / "bin"
            / "cudart64_12.dll"
        )
    for path_dir in os.environ.get("PATH", "").split(os.pathsep):
        if path_dir:
            candidates.append(Path(path_dir) / "cudart64_12.dll")
    for candidate in candidates:
        if candidate.is_file():
            return candidate
    return None


class CudaRuntime:
    def __init__(self) -> None:
        self._dll_dir_handles: list[Any] = []
        cuda_runtime_path = resolve_cuda_runtime_path()
        if cuda_runtime_path is not None and os.name == "nt":
            self._dll_dir_handles.append(os.add_dll_directory(str(cuda_runtime_path.parent)))
            self.lib = ctypes.WinDLL(str(cuda_runtime_path))
        else:
            self.lib = ctypes.WinDLL("cudart64_12.dll")
        self.cuda_malloc = self.lib.cudaMalloc
        self.cuda_malloc.argtypes = [ctypes.POINTER(ctypes.c_void_p), ctypes.c_size_t]
        self.cuda_malloc.restype = ctypes.c_int
        self.cuda_free = self.lib.cudaFree
        self.cuda_free.argtypes = [ctypes.c_void_p]
        self.cuda_free.restype = ctypes.c_int
        self.cuda_memcpy = self.lib.cudaMemcpy
        self.cuda_memcpy.argtypes = [ctypes.c_void_p, ctypes.c_void_p, ctypes.c_size_t, ctypes.c_int]
        self.cuda_memcpy.restype = ctypes.c_int

    def check(self, status: int, name: str) -> None:
        if status != CUDA_SUCCESS:
            raise RuntimeError(f"{name} failed with CUDA error {status}")

    def malloc(self, size: int) -> ctypes.c_void_p:
        ptr = ctypes.c_void_p()
        self.check(self.cuda_malloc(ctypes.byref(ptr), size), "cudaMalloc")
        return ptr

    def free(self, ptr: ctypes.c_void_p) -> None:
        if ptr:
            self.check(self.cuda_free(ptr), "cudaFree")

    def copy_host_to_device(self, dst: ctypes.c_void_p, src: np.ndarray) -> None:
        src = np.ascontiguousarray(src)
        self.check(
            self.cuda_memcpy(
                dst,
                ctypes.c_void_p(src.ctypes.data),
                src.nbytes,
                CUDA_MEMCPY_HOST_TO_DEVICE,
            ),
            "cudaMemcpyHostToDevice",
        )


def load_clip_preprocess_config(model_dir: Path) -> dict[str, Any]:
    config_path = model_dir / "open_clip_config.json"
    if not config_path.is_file():
        return {
            "size": 256,
            "mean": [0.0, 0.0, 0.0],
            "std": [1.0, 1.0, 1.0],
            "interpolation": "bilinear",
            "resize_mode": "shortest",
        }
    config = json.loads(config_path.read_text(encoding="utf-8"))
    preprocess = config.get("preprocess_cfg") or {}
    vision = ((config.get("model_cfg") or {}).get("vision_cfg") or {})
    return {
        "size": int(vision.get("image_size") or 256),
        "mean": [float(x) for x in preprocess.get("mean", [0.0, 0.0, 0.0])],
        "std": [float(x) for x in preprocess.get("std", [1.0, 1.0, 1.0])],
        "interpolation": str(preprocess.get("interpolation") or "bilinear"),
        "resize_mode": str(preprocess.get("resize_mode") or "shortest"),
    }


def load_pillow_image_module() -> Any:
    from PIL import Image

    return Image


def pil_resample(image_module: Any, name: str) -> int:
    if name == "bicubic":
        return image_module.Resampling.BICUBIC
    if name == "bilinear":
        return image_module.Resampling.BILINEAR
    return image_module.Resampling.NEAREST


def preprocess_image(path: Path, cfg: dict[str, Any], dtype: np.dtype[Any]) -> np.ndarray:
    image_module = load_pillow_image_module()
    size = int(cfg["size"])
    with image_module.open(path) as image:
        image = image.convert("RGB")
        resample = pil_resample(image_module, str(cfg["interpolation"]))
        if str(cfg["resize_mode"]) == "squash":
            image = image.resize((size, size), resample=resample)
        else:
            width, height = image.size
            scale = size / min(width, height)
            scaled_width = round(width * scale)
            scaled_height = round(height * scale)
            image = image.resize((scaled_width, scaled_height), resample=resample)
            left = round((scaled_width - size) / 2)
            top = round((scaled_height - size) / 2)
            image = image.crop((left, top, left + size, top + size))
        pixels = np.asarray(image, dtype=np.float32) / 255.0
    mean = np.asarray(cfg["mean"], dtype=np.float32).reshape(1, 1, 3)
    std = np.asarray(cfg["std"], dtype=np.float32).reshape(1, 1, 3)
    pixels = (pixels - mean) / std
    chw = np.transpose(pixels, (2, 0, 1))
    return np.ascontiguousarray(chw.astype(dtype, copy=False))


def manifest_image_paths(manifest: Path, limit: int) -> list[Path]:
    with sqlite3.connect(manifest) as con:
        rows = con.execute(
            """
            select path
            from image_assets
            where stale=0
              and (error is null or error='')
            order by
              case
                when source_class in ('chat_pic', 'file_recv', 'legacy_image', 'image') then 0
                else 1
              end,
              path
            limit ?
            """,
            (limit,),
        ).fetchall()
    return [Path(row[0]) for row in rows]


class ImageEntropyCalibrator:
    def __init__(
        self,
        trt: Any,
        *,
        manifest: Path,
        model_dir: Path,
        input_shape: list[int],
        input_dtype: Any,
        cache: Path,
        limit: int,
    ) -> None:
        trt.IInt8EntropyCalibrator2.__init__(self)
        if len(input_shape) != 4 or input_shape[1] != 3:
            raise RuntimeError(f"unsupported calibration input shape: {input_shape}")
        self.trt = trt
        self.batch_size = int(input_shape[0])
        self.input_shape = input_shape
        self.input_dtype = input_dtype
        self.numpy_dtype = np.float16 if input_dtype == trt.DataType.HALF else np.float32
        self.cache = cache
        self.cfg = load_clip_preprocess_config(model_dir)
        self.paths = manifest_image_paths(manifest, max(limit, self.batch_size))
        if not self.paths:
            raise RuntimeError(f"no calibration images found in manifest: {manifest}")
        self.index = 0
        self.cuda = CudaRuntime()
        self.host = np.zeros(tuple(input_shape), dtype=self.numpy_dtype)
        self.device = self.cuda.malloc(self.host.nbytes)
        self.batches_served = 0
        self.images_loaded = 0
        self.decode_errors = 0

    def get_batch_size(self) -> int:
        return self.batch_size

    def get_batch(self, names: list[str]) -> list[int] | None:
        filled = 0
        last_good: np.ndarray | None = None
        while filled < self.batch_size and self.index < len(self.paths):
            path = self.paths[self.index]
            self.index += 1
            try:
                image = preprocess_image(path, self.cfg, self.numpy_dtype)
            except Exception:
                self.decode_errors += 1
                continue
            self.host[filled] = image
            last_good = image
            filled += 1
        if filled == 0:
            return None
        if filled < self.batch_size:
            if last_good is not None:
                self.host[filled : self.batch_size] = last_good
            else:
                self.host[filled : self.batch_size] = 0
        self.cuda.copy_host_to_device(self.device, self.host)
        self.batches_served += 1
        self.images_loaded += filled
        return [int(self.device.value)]

    def read_calibration_cache(self) -> bytes | None:
        if self.cache.is_file():
            return self.cache.read_bytes()
        return None

    def write_calibration_cache(self, cache: bytes) -> None:
        self.cache.parent.mkdir(parents=True, exist_ok=True)
        self.cache.write_bytes(cache)

    def free(self) -> None:
        self.cuda.free(self.device)
        self.device = ctypes.c_void_p()


class NpyEntropyCalibrator:
    def __init__(
        self,
        trt: Any,
        *,
        npy: Path,
        input_shape: list[int],
        input_dtype: Any,
        cache: Path,
    ) -> None:
        trt.IInt8EntropyCalibrator2.__init__(self)
        if len(input_shape) != 4 or input_shape[1] != 3:
            raise RuntimeError(f"unsupported calibration input shape: {input_shape}")
        self.trt = trt
        self.batch_size = int(input_shape[0])
        self.input_shape = input_shape
        self.input_dtype = input_dtype
        self.numpy_dtype = np.float16 if input_dtype == trt.DataType.HALF else np.float32
        self.cache = cache
        self.npy = npy
        samples = np.load(npy, mmap_mode="r")
        if samples.ndim != 4:
            raise RuntimeError(f"calibration npy must be NCHW rank-4, got {samples.shape}")
        if tuple(int(x) for x in samples.shape[1:]) != tuple(input_shape[1:]):
            raise RuntimeError(
                f"calibration npy shape {samples.shape} does not match input shape {input_shape}"
            )
        if samples.shape[0] <= 0:
            raise RuntimeError(f"calibration npy has no samples: {npy}")
        self.samples = samples
        self.index = 0
        self.cuda = CudaRuntime()
        self.host = np.zeros(tuple(input_shape), dtype=self.numpy_dtype)
        self.device = self.cuda.malloc(self.host.nbytes)
        self.batches_served = 0
        self.images_loaded = 0

    def get_batch_size(self) -> int:
        return self.batch_size

    def get_batch(self, names: list[str]) -> list[int] | None:
        if self.index >= int(self.samples.shape[0]):
            return None
        end = min(self.index + self.batch_size, int(self.samples.shape[0]))
        batch = np.asarray(self.samples[self.index:end], dtype=self.numpy_dtype)
        filled = int(batch.shape[0])
        self.host[:filled] = batch
        if filled < self.batch_size:
            self.host[filled : self.batch_size] = batch[filled - 1]
        self.index = end
        self.cuda.copy_host_to_device(self.device, self.host)
        self.batches_served += 1
        self.images_loaded += filled
        return [int(self.device.value)]

    def read_calibration_cache(self) -> bytes | None:
        if self.cache.is_file():
            return self.cache.read_bytes()
        return None

    def write_calibration_cache(self, cache: bytes) -> None:
        self.cache.parent.mkdir(parents=True, exist_ok=True)
        self.cache.write_bytes(cache)

    def free(self) -> None:
        self.cuda.free(self.device)
        self.device = ctypes.c_void_p()


def make_image_entropy_calibrator(trt: Any, **kwargs: Any) -> ImageEntropyCalibrator:
    class _ImageEntropyCalibrator(ImageEntropyCalibrator, trt.IInt8EntropyCalibrator2):
        def __init__(self) -> None:
            ImageEntropyCalibrator.__init__(self, trt, **kwargs)

    return _ImageEntropyCalibrator()


def make_npy_entropy_calibrator(trt: Any, **kwargs: Any) -> NpyEntropyCalibrator:
    class _NpyEntropyCalibrator(NpyEntropyCalibrator, trt.IInt8EntropyCalibrator2):
        def __init__(self) -> None:
            NpyEntropyCalibrator.__init__(self, trt, **kwargs)

    return _NpyEntropyCalibrator()


def build_engine(args: argparse.Namespace) -> dict[str, Any]:
    repo = Path(__file__).resolve().parents[1]
    trt = load_tensorrt(repo)
    if os.name == "nt":
        cuda_path = os.environ.get("CUDA_PATH")
        if cuda_path:
            os.add_dll_directory(str(Path(cuda_path) / "bin"))
        trt_libs = repo / "output" / "_deps" / "tensorrt-cu12-py" / "tensorrt_libs"
        if trt_libs.is_dir():
            os.add_dll_directory(str(trt_libs))

    logger = trt.Logger(trt.Logger.WARNING)
    started = time.perf_counter()
    builder = trt.Builder(logger)
    flags = 1 << int(trt.NetworkDefinitionCreationFlag.EXPLICIT_BATCH)
    network = builder.create_network(flags)
    parser = trt.OnnxParser(network, logger)
    onnx_path = Path(args.onnx).resolve()
    previous_cwd = Path.cwd()
    try:
        os.chdir(onnx_path.parent)
        if hasattr(parser, "parse_from_file"):
            parsed = parser.parse_from_file(str(onnx_path.name))
        else:
            parsed = parser.parse(onnx_path.read_bytes())
    finally:
        os.chdir(previous_cwd)
    if not parsed:
        errors = [str(parser.get_error(index)) for index in range(parser.num_errors)]
        raise RuntimeError("TensorRT ONNX parse failed:\n" + "\n".join(errors))
    forced_fp16_layers = force_layer_fp16(trt, network, args.force_layer_fp16)

    config = builder.create_builder_config()
    config.set_memory_pool_limit(trt.MemoryPoolType.WORKSPACE, args.workspace_mib * 1024 * 1024)
    extra_memory_pools = [
        item
        for item in [
            set_memory_pool_mib_if_available(
                trt, config, "TACTIC_DRAM", args.tactic_dram_mib
            ),
            set_memory_pool_kib_if_available(
                trt, config, "TACTIC_SHARED_MEMORY", args.tactic_shared_memory_kib
            ),
        ]
        if item is not None
    ]
    tactic_sources = configure_tactic_sources(trt, config, args.disable_tactic_source)
    profiling_verbosity = set_profiling_verbosity(trt, config, args.profiling_verbosity)
    if args.builder_opt_level is not None:
        config.builder_optimization_level = args.builder_opt_level
    if args.fp16:
        config.set_flag(trt.BuilderFlag.FP16)
    if args.int8:
        config.set_flag(trt.BuilderFlag.INT8)
    calibrator = None
    if args.int8 and args.calibration_npy:
        input_tensor = network.get_input(0)
        calibration_cache = args.calibration_cache or args.out.with_suffix(".calibration.cache")
        calibrator = make_npy_entropy_calibrator(
            trt,
            npy=args.calibration_npy.resolve(),
            input_shape=tensor_shape(input_tensor),
            input_dtype=input_tensor.dtype,
            cache=calibration_cache.resolve(),
        )
        config.int8_calibrator = calibrator
    elif args.int8 and args.calibration_manifest:
        input_tensor = network.get_input(0)
        calibration_cache = args.calibration_cache or args.out.with_suffix(".calibration.cache")
        calibrator = make_image_entropy_calibrator(
            trt,
            manifest=args.calibration_manifest.resolve(),
            model_dir=onnx_path.parent,
            input_shape=tensor_shape(input_tensor),
            input_dtype=input_tensor.dtype,
            cache=calibration_cache.resolve(),
            limit=args.calibration_limit,
        )
        config.int8_calibrator = calibrator
    if args.obey_precision_constraints:
        config.set_flag(trt.BuilderFlag.OBEY_PRECISION_CONSTRAINTS)
    prefer_precision_constraints = False
    if args.prefer_precision_constraints:
        prefer_precision_constraints = set_builder_flag_if_available(
            trt, config, "PREFER_PRECISION_CONSTRAINTS"
        )
    monitor_memory = False
    if args.monitor_memory:
        monitor_memory = set_builder_flag_if_available(trt, config, "MONITOR_MEMORY")

    try:
        serialized = builder.build_serialized_network(network, config)
        if serialized is None:
            raise RuntimeError("TensorRT build_serialized_network returned None")
    finally:
        if calibrator is not None:
            calibrator.free()

    args.out.parent.mkdir(parents=True, exist_ok=True)
    args.out.write_bytes(bytes(serialized))
    elapsed_ms = (time.perf_counter() - started) * 1000.0

    return {
        "onnx": str(onnx_path),
        "engine": str(args.out.resolve()),
        "engine_bytes": args.out.stat().st_size,
        "build_ms": elapsed_ms,
        "fp16": args.fp16,
        "int8": args.int8,
        "obey_precision_constraints": args.obey_precision_constraints,
        "prefer_precision_constraints": prefer_precision_constraints,
        "monitor_memory": monitor_memory,
        "workspace_mib": args.workspace_mib,
        "extra_memory_pools": extra_memory_pools,
        "tactic_sources": tactic_sources,
        "builder_opt_level": args.builder_opt_level,
        "profiling_verbosity": profiling_verbosity,
        "forced_fp16_layers": forced_fp16_layers,
        "calibration": None
        if calibrator is None
        else {
            "source": "npy" if args.calibration_npy else "manifest",
            "manifest": str(args.calibration_manifest.resolve()) if args.calibration_manifest else None,
            "npy": str(args.calibration_npy.resolve()) if args.calibration_npy else None,
            "cache": str((args.calibration_cache or args.out.with_suffix(".calibration.cache")).resolve()),
            "limit": args.calibration_limit,
            "batch_size": calibrator.batch_size,
            "available_paths": len(getattr(calibrator, "paths", [])),
            "available_samples": int(getattr(calibrator, "samples", np.empty((0,))).shape[0])
            if hasattr(calibrator, "samples")
            else None,
            "batches_served": calibrator.batches_served,
            "images_loaded": calibrator.images_loaded,
            "decode_errors": getattr(calibrator, "decode_errors", 0),
            "input_dtype": str(calibrator.input_dtype),
        },
        "inputs": [
            {
                "index": index,
                "name": network.get_input(index).name,
                "shape": tensor_shape(network.get_input(index)),
                "dtype": str(network.get_input(index).dtype),
            }
            for index in range(network.num_inputs)
        ],
        "outputs": [
            {
                "index": index,
                "name": network.get_output(index).name,
                "shape": tensor_shape(network.get_output(index)),
                "dtype": str(network.get_output(index).dtype),
            }
            for index in range(network.num_outputs)
        ],
    }


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--onnx", required=True, type=Path)
    parser.add_argument("--out", required=True, type=Path)
    parser.add_argument("--report", type=Path)
    parser.add_argument("--workspace-mib", type=int, default=8192)
    parser.add_argument("--tactic-dram-mib", type=int)
    parser.add_argument("--tactic-shared-memory-kib", type=int)
    parser.add_argument("--fp16", action="store_true")
    parser.add_argument("--int8", action="store_true")
    parser.add_argument("--builder-opt-level", type=int, choices=range(0, 6))
    parser.add_argument("--calibration-manifest", type=Path)
    parser.add_argument("--calibration-npy", type=Path)
    parser.add_argument("--calibration-limit", type=int, default=512)
    parser.add_argument("--calibration-cache", type=Path)
    parser.add_argument("--obey-precision-constraints", action="store_true")
    parser.add_argument("--prefer-precision-constraints", action="store_true")
    parser.add_argument("--force-layer-fp16", action="append", default=[])
    parser.add_argument("--monitor-memory", action="store_true")
    parser.add_argument("--disable-tactic-source", action="append", default=[])
    parser.add_argument(
        "--profiling-verbosity",
        choices=("default", "layer_names_only", "detailed"),
        default="detailed",
    )
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    report = build_engine(args)
    text = json.dumps(report, ensure_ascii=False, indent=2)
    if args.report:
        args.report.parent.mkdir(parents=True, exist_ok=True)
        args.report.write_text(text + "\n", encoding="utf-8")
    print(text)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
