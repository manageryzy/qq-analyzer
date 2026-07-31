from __future__ import annotations

import argparse
import ctypes
import json
import os
import statistics
import sys
import time
from pathlib import Path
from typing import Any


CUDA_SUCCESS = 0


class CudaError(RuntimeError):
    pass


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
        self.cuda_memset = self.lib.cudaMemset
        self.cuda_memset.argtypes = [ctypes.c_void_p, ctypes.c_int, ctypes.c_size_t]
        self.cuda_memset.restype = ctypes.c_int
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
            raise CudaError(f"{name} failed with CUDA error {status}")

    def malloc(self, size: int) -> ctypes.c_void_p:
        ptr = ctypes.c_void_p()
        self.check(self.cuda_malloc(ctypes.byref(ptr), size), "cudaMalloc")
        return ptr

    def memset(self, ptr: ctypes.c_void_p, value: int, size: int) -> None:
        self.check(self.cuda_memset(ptr, value, size), "cudaMemset")

    def free(self, ptr: ctypes.c_void_p) -> None:
        if ptr:
            self.check(self.cuda_free(ptr), "cudaFree")

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
        ms = ctypes.c_float()
        self.check(
            self.cuda_event_elapsed_time(ctypes.byref(ms), start, stop),
            "cudaEventElapsedTime",
        )
        return float(ms.value)


def load_tensorrt(repo: Path) -> Any:
    trt_py = repo / "output" / "_deps" / "tensorrt-cu12-py"
    if trt_py.is_dir():
        sys.path.insert(0, str(trt_py))
    import tensorrt as trt

    return trt


def dtype_size(trt: Any, dtype: Any) -> int:
    if dtype == trt.DataType.FLOAT:
        return 4
    if dtype == trt.DataType.HALF:
        return 2
    if dtype in (trt.DataType.INT32,):
        return 4
    if dtype in (trt.DataType.INT8, trt.DataType.BOOL):
        return 1
    raise ValueError(f"unsupported TensorRT dtype: {dtype}")


def shape_numel(shape: tuple[int, ...]) -> int:
    numel = 1
    for dim in shape:
        if dim < 0:
            raise ValueError(f"dynamic dimension is not supported in native benchmark: {shape}")
        numel *= dim
    return numel


def percentile(values: list[float], pct: float) -> float:
    if not values:
        return 0.0
    ordered = sorted(values)
    index = min(len(ordered) - 1, max(0, round((len(ordered) - 1) * pct)))
    return ordered[index]


def benchmark(args: argparse.Namespace) -> dict[str, Any]:
    repo = Path(__file__).resolve().parents[1]
    trt = load_tensorrt(repo)
    cuda = CudaRuntime()
    logger = trt.Logger(trt.Logger.WARNING)
    started = time.perf_counter()
    runtime = trt.Runtime(logger)
    engine_bytes = Path(args.engine).read_bytes()
    engine = runtime.deserialize_cuda_engine(engine_bytes)
    if engine is None:
        raise RuntimeError(f"failed to deserialize TensorRT engine: {args.engine}")
    context = engine.create_execution_context()
    if context is None:
        raise RuntimeError("failed to create TensorRT execution context")
    load_ms = (time.perf_counter() - started) * 1000.0

    stream = cuda.stream_create()
    start_event = cuda.event_create()
    stop_event = cuda.event_create()
    buffers: list[ctypes.c_void_p] = []
    tensors: list[dict[str, Any]] = []
    input_batch = 0
    try:
        for index in range(engine.num_io_tensors):
            name = engine.get_tensor_name(index)
            mode = engine.get_tensor_mode(name)
            shape = tuple(int(x) for x in engine.get_tensor_shape(name))
            dtype = engine.get_tensor_dtype(name)
            size = shape_numel(shape) * dtype_size(trt, dtype)
            ptr = cuda.malloc(size)
            cuda.memset(ptr, 0, size)
            buffers.append(ptr)
            ok = context.set_tensor_address(name, int(ptr.value))
            if not ok:
                raise RuntimeError(f"set_tensor_address failed for {name}")
            is_input = mode == trt.TensorIOMode.INPUT
            if is_input and shape:
                input_batch = max(input_batch, int(shape[0]))
            tensors.append(
                {
                    "name": name,
                    "mode": "input" if is_input else "output",
                    "shape": shape,
                    "dtype": str(dtype),
                    "bytes": size,
                }
            )

        for _ in range(args.warmup):
            if not context.execute_async_v3(int(stream.value)):
                raise RuntimeError("execute_async_v3 warmup failed")
        cuda.stream_synchronize(stream)

        samples: list[float] = []
        wall_started = time.perf_counter()
        for _ in range(args.iterations):
            cuda.event_record(start_event, stream)
            if not context.execute_async_v3(int(stream.value)):
                raise RuntimeError("execute_async_v3 failed")
            cuda.event_record(stop_event, stream)
            cuda.event_synchronize(stop_event)
            samples.append(cuda.event_elapsed_ms(start_event, stop_event))
        cuda.stream_synchronize(stream)
        wall_ms = (time.perf_counter() - wall_started) * 1000.0
    finally:
        for ptr in buffers:
            cuda.free(ptr)
        cuda.event_destroy(start_event)
        cuda.event_destroy(stop_event)
        cuda.stream_destroy(stream)

    mean_ms = statistics.fmean(samples) if samples else 0.0
    median_ms = statistics.median(samples) if samples else 0.0
    return {
        "engine": str(Path(args.engine).resolve()),
        "engine_bytes": len(engine_bytes),
        "load_ms": load_ms,
        "warmup": args.warmup,
        "iterations": args.iterations,
        "input_batch": input_batch,
        "tensors": tensors,
        "event_mean_ms": mean_ms,
        "event_median_ms": median_ms,
        "event_min_ms": min(samples) if samples else 0.0,
        "event_max_ms": max(samples) if samples else 0.0,
        "event_p90_ms": percentile(samples, 0.90),
        "event_p99_ms": percentile(samples, 0.99),
        "wall_ms": wall_ms,
        "event_images_per_second": (input_batch * 1000.0 / mean_ms) if mean_ms else 0.0,
        "wall_images_per_second": (input_batch * args.iterations * 1000.0 / wall_ms)
        if wall_ms
        else 0.0,
        "samples_ms": samples[: args.keep_samples],
    }


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--engine", required=True, type=Path)
    parser.add_argument("--warmup", type=int, default=20)
    parser.add_argument("--iterations", type=int, default=100)
    parser.add_argument("--keep-samples", type=int, default=20)
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
