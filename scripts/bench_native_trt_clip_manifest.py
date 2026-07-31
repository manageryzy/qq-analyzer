from __future__ import annotations

import argparse
import ctypes
import json
import os
import sqlite3
import statistics
import sys
import time
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path
from typing import Any


CUDA_SUCCESS = 0
CUDA_MEMCPY_HOST_TO_DEVICE = 1
CUDA_MEMCPY_DEVICE_TO_HOST = 2


def add_project_python_paths(repo: Path) -> None:
    for rel in (
        ("output", "_deps", "windows-py312-native-trt"),
        ("output", "_deps", "tensorrt-cu12-py"),
    ):
        path = repo.joinpath(*rel)
        if path.is_dir():
            sys.path.insert(0, str(path))


def add_windows_dll_dirs(repo: Path) -> None:
    if os.name != "nt":
        return
    for path in (
        repo / "output" / "_deps" / "tensorrt-cu12-py" / "tensorrt_libs",
        Path(os.environ.get("CUDA_PATH", "")) / "bin",
        Path("C:/Program Files/NVIDIA GPU Computing Toolkit/CUDA/v12.5/bin"),
    ):
        if path.is_dir():
            os.add_dll_directory(str(path))


def load_tensorrt(repo: Path) -> Any:
    add_project_python_paths(repo)
    add_windows_dll_dirs(repo)
    import tensorrt as trt

    return trt


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
        cuda_runtime_path = resolve_cuda_runtime_path()
        if cuda_runtime_path is not None and os.name == "nt":
            self.lib = ctypes.WinDLL(str(cuda_runtime_path))
        else:
            self.lib = ctypes.WinDLL("cudart64_12.dll")
        self.cuda_malloc = self.lib.cudaMalloc
        self.cuda_malloc.argtypes = [ctypes.POINTER(ctypes.c_void_p), ctypes.c_size_t]
        self.cuda_malloc.restype = ctypes.c_int
        self.cuda_free = self.lib.cudaFree
        self.cuda_free.argtypes = [ctypes.c_void_p]
        self.cuda_free.restype = ctypes.c_int
        self.cuda_memset = self.lib.cudaMemset
        self.cuda_memset.argtypes = [ctypes.c_void_p, ctypes.c_int, ctypes.c_size_t]
        self.cuda_memset.restype = ctypes.c_int
        self.cuda_memcpy_async = self.lib.cudaMemcpyAsync
        self.cuda_memcpy_async.argtypes = [
            ctypes.c_void_p,
            ctypes.c_void_p,
            ctypes.c_size_t,
            ctypes.c_int,
            ctypes.c_void_p,
        ]
        self.cuda_memcpy_async.restype = ctypes.c_int
        self.cuda_stream_create = self.lib.cudaStreamCreate
        self.cuda_stream_create.argtypes = [ctypes.POINTER(ctypes.c_void_p)]
        self.cuda_stream_create.restype = ctypes.c_int
        self.cuda_stream_destroy = self.lib.cudaStreamDestroy
        self.cuda_stream_destroy.argtypes = [ctypes.c_void_p]
        self.cuda_stream_destroy.restype = ctypes.c_int
        self.cuda_stream_synchronize = self.lib.cudaStreamSynchronize
        self.cuda_stream_synchronize.argtypes = [ctypes.c_void_p]
        self.cuda_stream_synchronize.restype = ctypes.c_int
        self.cuda_event_create = self.lib.cudaEventCreate
        self.cuda_event_create.argtypes = [ctypes.POINTER(ctypes.c_void_p)]
        self.cuda_event_create.restype = ctypes.c_int
        self.cuda_event_destroy = self.lib.cudaEventDestroy
        self.cuda_event_destroy.argtypes = [ctypes.c_void_p]
        self.cuda_event_destroy.restype = ctypes.c_int
        self.cuda_event_record = self.lib.cudaEventRecord
        self.cuda_event_record.argtypes = [ctypes.c_void_p, ctypes.c_void_p]
        self.cuda_event_record.restype = ctypes.c_int
        self.cuda_event_synchronize = self.lib.cudaEventSynchronize
        self.cuda_event_synchronize.argtypes = [ctypes.c_void_p]
        self.cuda_event_synchronize.restype = ctypes.c_int
        self.cuda_event_elapsed_time = self.lib.cudaEventElapsedTime
        self.cuda_event_elapsed_time.argtypes = [
            ctypes.POINTER(ctypes.c_float),
            ctypes.c_void_p,
            ctypes.c_void_p,
        ]
        self.cuda_event_elapsed_time.restype = ctypes.c_int

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

    def memset(self, ptr: ctypes.c_void_p, value: int, size: int) -> None:
        self.check(self.cuda_memset(ptr, value, size), "cudaMemset")

    def memcpy_async(
        self,
        dst: ctypes.c_void_p | int,
        src: ctypes.c_void_p | int,
        size: int,
        kind: int,
        stream: ctypes.c_void_p,
    ) -> None:
        dst_ptr = dst if isinstance(dst, ctypes.c_void_p) else ctypes.c_void_p(dst)
        src_ptr = src if isinstance(src, ctypes.c_void_p) else ctypes.c_void_p(src)
        self.check(self.cuda_memcpy_async(dst_ptr, src_ptr, size, kind, stream), "cudaMemcpyAsync")

    def stream_create(self) -> ctypes.c_void_p:
        stream = ctypes.c_void_p()
        self.check(self.cuda_stream_create(ctypes.byref(stream)), "cudaStreamCreate")
        return stream

    def stream_destroy(self, stream: ctypes.c_void_p) -> None:
        self.check(self.cuda_stream_destroy(stream), "cudaStreamDestroy")

    def stream_synchronize(self, stream: ctypes.c_void_p) -> None:
        self.check(self.cuda_stream_synchronize(stream), "cudaStreamSynchronize")

    def event_create(self) -> ctypes.c_void_p:
        event = ctypes.c_void_p()
        self.check(self.cuda_event_create(ctypes.byref(event)), "cudaEventCreate")
        return event

    def event_destroy(self, event: ctypes.c_void_p) -> None:
        self.check(self.cuda_event_destroy(event), "cudaEventDestroy")

    def event_record(self, event: ctypes.c_void_p, stream: ctypes.c_void_p) -> None:
        self.check(self.cuda_event_record(event, stream), "cudaEventRecord")

    def event_synchronize(self, event: ctypes.c_void_p) -> None:
        self.check(self.cuda_event_synchronize(event), "cudaEventSynchronize")

    def event_elapsed_ms(self, start: ctypes.c_void_p, stop: ctypes.c_void_p) -> float:
        elapsed = ctypes.c_float()
        self.check(
            self.cuda_event_elapsed_time(ctypes.byref(elapsed), start, stop),
            "cudaEventElapsedTime",
        )
        return float(elapsed.value)


def dtype_size(trt: Any, dtype: Any) -> int:
    if dtype == trt.DataType.FLOAT:
        return 4
    if dtype == trt.DataType.HALF:
        return 2
    if dtype == trt.DataType.INT32:
        return 4
    if dtype in (trt.DataType.INT8, trt.DataType.BOOL):
        return 1
    raise ValueError(f"unsupported TensorRT dtype: {dtype}")


def numpy_dtype_for_trt(trt: Any, dtype: Any) -> Any:
    import numpy as np

    if dtype == trt.DataType.FLOAT:
        return np.float32
    if dtype == trt.DataType.HALF:
        return np.float16
    if dtype == trt.DataType.INT32:
        return np.int32
    if dtype == trt.DataType.INT8:
        return np.int8
    raise ValueError(f"unsupported TensorRT dtype for numpy buffer: {dtype}")


def shape_numel(shape: tuple[int, ...]) -> int:
    numel = 1
    for dim in shape:
        if dim < 0:
            raise ValueError(f"dynamic shape is not supported: {shape}")
        numel *= dim
    return numel


def percentile(values: list[float], pct: float) -> float:
    if not values:
        return 0.0
    ordered = sorted(values)
    index = min(len(ordered) - 1, max(0, round((len(ordered) - 1) * pct)))
    return ordered[index]


def load_clip_preprocess_config(model_dir: Path) -> dict[str, Any]:
    config = json.loads((model_dir / "open_clip_config.json").read_text(encoding="utf-8"))
    preprocess = config["preprocess_cfg"]
    return {
        "image_size": int(config["model_cfg"]["vision_cfg"]["image_size"]),
        "mean": [float(value) for value in preprocess.get("mean", [0.0, 0.0, 0.0])],
        "std": [float(value) for value in preprocess.get("std", [1.0, 1.0, 1.0])],
        "interpolation": str(preprocess.get("interpolation", "bicubic")),
        "resize_mode": str(preprocess.get("resize_mode", "shortest")),
    }


def select_paths(manifest: Path, limit: int) -> list[str]:
    with sqlite3.connect(str(manifest)) as con:
        rows = con.execute(
            """
            select path
            from image_assets
            where stale=0 and coalesce(error, '')=''
            order by path
            limit ?
            """,
            (limit,),
        ).fetchall()
    return [str(row[0]) for row in rows]


def preprocess_one(path: str, cfg: dict[str, Any]) -> Any:
    import numpy as np
    from PIL import Image

    size = int(cfg["image_size"])
    interpolation = str(cfg["interpolation"]).lower()
    resize_mode = str(cfg["resize_mode"]).lower()
    resample = Image.Resampling.BICUBIC
    if interpolation == "bilinear":
        resample = Image.Resampling.BILINEAR
    elif interpolation == "nearest":
        resample = Image.Resampling.NEAREST

    with Image.open(path) as image:
        image = image.convert("RGB")
        width, height = image.size
        if resize_mode == "squash":
            image = image.resize((size, size), resample=resample)
        else:
            scale = size / min(width, height)
            scaled_width = round(width * scale)
            scaled_height = round(height * scale)
            image = image.resize((scaled_width, scaled_height), resample=resample)
            left = round((scaled_width - size) / 2)
            top = round((scaled_height - size) / 2)
            image = image.crop((left, top, left + size, top + size))
        array = np.asarray(image, dtype=np.float32) / 255.0

    mean = np.asarray(cfg["mean"], dtype=np.float32)
    std = np.asarray(cfg["std"], dtype=np.float32)
    array = (array - mean) / std
    return np.transpose(array, (2, 0, 1))


def preprocess_batch(
    paths: list[str],
    cfg: dict[str, Any],
    batch_size: int,
    input_dtype: Any,
    workers: int,
) -> tuple[Any, int, int]:
    import numpy as np

    size = int(cfg["image_size"])
    tensor = np.zeros((batch_size, 3, size, size), dtype=input_dtype)
    ok = 0
    errors = 0
    if workers <= 1 or len(paths) <= 1:
        for index, path in enumerate(paths):
            try:
                tensor[index] = preprocess_one(path, cfg).astype(input_dtype, copy=False)
                ok += 1
            except Exception:
                errors += 1
        return np.ascontiguousarray(tensor), ok, errors

    with ThreadPoolExecutor(max_workers=workers) as executor:
        futures = [executor.submit(preprocess_one, path, cfg) for path in paths]
        for index, future in enumerate(futures):
            try:
                tensor[index] = future.result().astype(input_dtype, copy=False)
                ok += 1
            except Exception:
                errors += 1
    return np.ascontiguousarray(tensor), ok, errors


class NativeTrtClip:
    def __init__(self, repo: Path, engine_path: Path) -> None:
        self.trt = load_tensorrt(repo)
        self.cuda = CudaRuntime()
        logger = self.trt.Logger(self.trt.Logger.WARNING)
        started = time.perf_counter()
        self.runtime = self.trt.Runtime(logger)
        engine_bytes = engine_path.read_bytes()
        self.engine = self.runtime.deserialize_cuda_engine(engine_bytes)
        if self.engine is None:
            raise RuntimeError(f"failed to deserialize engine: {engine_path}")
        self.context = self.engine.create_execution_context()
        if self.context is None:
            raise RuntimeError("failed to create TensorRT execution context")
        self.load_ms = (time.perf_counter() - started) * 1000.0
        self.stream = self.cuda.stream_create()
        self.start_event = self.cuda.event_create()
        self.stop_event = self.cuda.event_create()
        self.buffers: list[ctypes.c_void_p] = []
        self.tensors: list[dict[str, Any]] = []
        self.input_name = ""
        self.output_name = ""
        self.input_shape: tuple[int, ...] = ()
        self.output_shape: tuple[int, ...] = ()
        self.input_dtype = None
        self.output_dtype = None
        for index in range(self.engine.num_io_tensors):
            name = self.engine.get_tensor_name(index)
            mode = self.engine.get_tensor_mode(name)
            shape = tuple(int(dim) for dim in self.engine.get_tensor_shape(name))
            dtype = self.engine.get_tensor_dtype(name)
            size = shape_numel(shape) * dtype_size(self.trt, dtype)
            ptr = self.cuda.malloc(size)
            self.cuda.memset(ptr, 0, size)
            self.buffers.append(ptr)
            if not self.context.set_tensor_address(name, int(ptr.value)):
                raise RuntimeError(f"set_tensor_address failed for {name}")
            is_input = mode == self.trt.TensorIOMode.INPUT
            if is_input:
                self.input_name = name
                self.input_shape = shape
                self.input_dtype = dtype
                self.input_ptr = ptr
                self.input_bytes = size
            else:
                self.output_name = name
                self.output_shape = shape
                self.output_dtype = dtype
                self.output_ptr = ptr
                self.output_bytes = size
            self.tensors.append(
                {
                    "name": name,
                    "mode": "input" if is_input else "output",
                    "shape": shape,
                    "dtype": str(dtype),
                    "bytes": size,
                }
            )
        if not self.input_name or not self.output_name:
            raise RuntimeError("engine must have one input and one output tensor")

    def close(self) -> None:
        for ptr in self.buffers:
            self.cuda.free(ptr)
        self.cuda.event_destroy(self.start_event)
        self.cuda.event_destroy(self.stop_event)
        self.cuda.stream_destroy(self.stream)

    def run_batch(self, input_array: Any, output_array: Any) -> tuple[float, float, float]:
        h2d_start = time.perf_counter()
        self.cuda.memcpy_async(
            self.input_ptr,
            int(input_array.ctypes.data),
            int(input_array.nbytes),
            CUDA_MEMCPY_HOST_TO_DEVICE,
            self.stream,
        )
        self.cuda.stream_synchronize(self.stream)
        h2d_ms = (time.perf_counter() - h2d_start) * 1000.0

        self.cuda.event_record(self.start_event, self.stream)
        if not self.context.execute_async_v3(int(self.stream.value)):
            raise RuntimeError("execute_async_v3 failed")
        self.cuda.event_record(self.stop_event, self.stream)
        self.cuda.event_synchronize(self.stop_event)
        run_ms = self.cuda.event_elapsed_ms(self.start_event, self.stop_event)

        d2h_start = time.perf_counter()
        self.cuda.memcpy_async(
            int(output_array.ctypes.data),
            self.output_ptr,
            int(output_array.nbytes),
            CUDA_MEMCPY_DEVICE_TO_HOST,
            self.stream,
        )
        self.cuda.stream_synchronize(self.stream)
        d2h_ms = (time.perf_counter() - d2h_start) * 1000.0
        return h2d_ms, run_ms, d2h_ms


def benchmark(args: argparse.Namespace) -> dict[str, Any]:
    repo = Path(__file__).resolve().parents[1]
    add_project_python_paths(repo)
    import numpy as np

    cfg = load_clip_preprocess_config(args.model_dir)
    paths = select_paths(args.manifest, args.limit)
    clip = NativeTrtClip(repo, args.engine)
    batch_size = int(clip.input_shape[0])
    input_dtype = numpy_dtype_for_trt(clip.trt, clip.input_dtype)
    output_dtype = numpy_dtype_for_trt(clip.trt, clip.output_dtype)
    output_array = np.empty(clip.output_shape, dtype=output_dtype)

    total_started = time.perf_counter()
    h2d_samples: list[float] = []
    run_samples: list[float] = []
    d2h_samples: list[float] = []
    preprocess_samples: list[float] = []
    batch_wall_samples: list[float] = []
    processed = 0
    errors = 0
    try:
        for batch_start in range(0, len(paths), batch_size):
            batch_paths = paths[batch_start : batch_start + batch_size]
            batch_wall_started = time.perf_counter()
            preprocess_started = time.perf_counter()
            input_array, ok, batch_errors = preprocess_batch(
                batch_paths,
                cfg,
                batch_size,
                input_dtype,
                args.preprocess_workers,
            )
            preprocess_samples.append((time.perf_counter() - preprocess_started) * 1000.0)
            h2d_ms, run_ms, d2h_ms = clip.run_batch(input_array, output_array)
            batch_wall_samples.append((time.perf_counter() - batch_wall_started) * 1000.0)
            h2d_samples.append(h2d_ms)
            run_samples.append(run_ms)
            d2h_samples.append(d2h_ms)
            processed += ok
            errors += batch_errors
    finally:
        clip.close()
    total_ms = (time.perf_counter() - total_started) * 1000.0

    def stats(values: list[float]) -> dict[str, float]:
        if not values:
            return {"sum_ms": 0.0, "mean_ms": 0.0, "median_ms": 0.0, "p90_ms": 0.0}
        return {
            "sum_ms": float(sum(values)),
            "mean_ms": float(statistics.fmean(values)),
            "median_ms": float(statistics.median(values)),
            "p90_ms": float(percentile(values, 0.90)),
        }

    return {
        "manifest": str(args.manifest.resolve()),
        "engine": str(args.engine.resolve()),
        "model_dir": str(args.model_dir.resolve()),
        "limit": args.limit,
        "selected_paths": len(paths),
        "processed_images": processed,
        "errors": errors,
        "batch_size": batch_size,
        "batches": len(run_samples),
        "load_engine_ms": clip.load_ms,
        "total_ms": total_ms,
        "end_to_end_images_per_second": processed * 1000.0 / total_ms if total_ms else 0.0,
        "run_images_per_second_mean_batch": batch_size * 1000.0 / statistics.fmean(run_samples)
        if run_samples
        else 0.0,
        "input_tensor": next((t for t in clip.tensors if t["mode"] == "input"), None),
        "output_tensor": next((t for t in clip.tensors if t["mode"] == "output"), None),
        "preprocess": stats(preprocess_samples),
        "h2d": stats(h2d_samples),
        "run": stats(run_samples),
        "d2h": stats(d2h_samples),
        "batch_wall": stats(batch_wall_samples),
        "samples": {
            "preprocess_ms": preprocess_samples[: args.keep_samples],
            "h2d_ms": h2d_samples[: args.keep_samples],
            "run_ms": run_samples[: args.keep_samples],
            "d2h_ms": d2h_samples[: args.keep_samples],
            "batch_wall_ms": batch_wall_samples[: args.keep_samples],
        },
    }


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--manifest", required=True, type=Path)
    parser.add_argument("--engine", required=True, type=Path)
    parser.add_argument("--model-dir", required=True, type=Path)
    parser.add_argument("--limit", type=int, default=2048)
    parser.add_argument("--preprocess-workers", type=int, default=max(1, (os.cpu_count() or 8) // 2))
    parser.add_argument("--keep-samples", type=int, default=10)
    parser.add_argument("--out", type=Path)
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    report = benchmark(args)
    text = json.dumps(report, ensure_ascii=False, indent=2)
    if args.out:
        args.out.parent.mkdir(parents=True, exist_ok=True)
        args.out.write_text(text + "\n", encoding="utf-8")
    print(text)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
