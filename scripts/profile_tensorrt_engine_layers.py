from __future__ import annotations

import argparse
import ctypes
import json
import statistics
import time
from pathlib import Path
from typing import Any

from bench_tensorrt_engine import CudaRuntime, dtype_size, load_tensorrt, percentile, shape_numel


def make_layer_profiler(trt: Any) -> Any:
    class LayerProfiler(trt.IProfiler):
        def __init__(self) -> None:
            trt.IProfiler.__init__(self)
            self.records: list[tuple[str, float]] = []

        def report_layer_time(self, layer_name: str, ms: float) -> None:
            self.records.append((layer_name, float(ms)))

    return LayerProfiler()


def classify_layer(name: str) -> str:
    lowered = name.lower()
    if "reformatting" in lowered or "copy" in lowered:
        return "reformat_copy"
    if "softmax" in lowered:
        return "softmax"
    if "gelu" in lowered and ("conv" in lowered or "pwn" in lowered):
        return "conv_or_pointwise_gelu"
    if "conv" in lowered:
        return "conv"
    if "matmul" in lowered or "gemm" in lowered or "addmm" in lowered or "mm(" in lowered:
        return "matmul"
    if "cast" in lowered:
        return "cast"
    if "shuffle" in lowered or "transpose" in lowered or "reshape" in lowered:
        return "reshape"
    if "pwn" in lowered or "elementwise" in lowered or "add" in lowered or "mul" in lowered:
        return "pointwise"
    return "other"


def summarize_layers(records: list[tuple[str, float]], keep: int) -> dict[str, Any]:
    by_name: dict[str, list[float]] = {}
    for name, ms in records:
        by_name.setdefault(name, []).append(ms)
    rows: list[dict[str, Any]] = []
    for name, values in by_name.items():
        total = sum(values)
        rows.append(
            {
                "name": name,
                "count": len(values),
                "total_ms": total,
                "mean_ms": statistics.fmean(values),
                "median_ms": statistics.median(values),
                "min_ms": min(values),
                "max_ms": max(values),
            }
        )
    rows.sort(key=lambda row: row["total_ms"], reverse=True)
    grand_total = sum(row["total_ms"] for row in rows)
    if grand_total:
        for row in rows:
            row["total_pct"] = row["total_ms"] * 100.0 / grand_total
            row["category"] = classify_layer(row["name"])

    by_category: dict[str, dict[str, Any]] = {}
    for row in rows:
        category = row["category"]
        entry = by_category.setdefault(
            category,
            {"category": category, "layers": 0, "records": 0, "total_ms": 0.0},
        )
        entry["layers"] += 1
        entry["records"] += row["count"]
        entry["total_ms"] += row["total_ms"]
    category_rows = sorted(by_category.values(), key=lambda row: row["total_ms"], reverse=True)
    if grand_total:
        for row in category_rows:
            row["total_pct"] = row["total_ms"] * 100.0 / grand_total

    return {
        "unique_layers": len(rows),
        "total_ms": grand_total,
        "categories": category_rows,
        "top_layers": rows[:keep],
    }


def engine_inspection(trt: Any, engine: Any, context: Any) -> dict[str, Any]:
    inspector = None
    try:
        inspector = engine.create_engine_inspector()
    except Exception as exc:
        return {"available": False, "error": f"{type(exc).__name__}: {exc}"}
    if inspector is None:
        return {"available": False, "error": "create_engine_inspector returned None"}
    result: dict[str, Any] = {"available": True}
    try:
        inspector.execution_context = context
    except Exception as exc:
        result["set_context_error"] = f"{type(exc).__name__}: {exc}"
    for label, fmt_name in (("json", "JSON"), ("text", "ONELINE")):
        try:
            fmt = getattr(trt.LayerInformationFormat, fmt_name)
            info = inspector.get_engine_information(fmt)
        except Exception as exc:
            result[f"{label}_error"] = f"{type(exc).__name__}: {exc}"
        else:
            result[label] = info
    return result


def profile(args: argparse.Namespace) -> dict[str, Any]:
    repo = Path(__file__).resolve().parents[1]
    trt = load_tensorrt(repo)
    cuda = CudaRuntime()
    logger = trt.Logger(trt.Logger.WARNING)
    started = time.perf_counter()
    runtime = trt.Runtime(logger)
    engine_path = Path(args.engine)
    engine_bytes = engine_path.read_bytes()
    engine = runtime.deserialize_cuda_engine(engine_bytes)
    if engine is None:
        raise RuntimeError(f"failed to deserialize TensorRT engine: {engine_path}")
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
    profiler = make_layer_profiler(trt)
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
                    "shape": list(shape),
                    "dtype": str(dtype),
                    "bytes": size,
                }
            )

        for _ in range(args.warmup):
            if not context.execute_async_v3(int(stream.value)):
                raise RuntimeError("execute_async_v3 warmup failed")
        cuda.stream_synchronize(stream)

        context.profiler = profiler
        context.enqueue_emits_profile = args.enqueue_emits_profile

        samples: list[float] = []
        wall_started = time.perf_counter()
        for _ in range(args.iterations):
            cuda.event_record(start_event, stream)
            if not context.execute_async_v3(int(stream.value)):
                raise RuntimeError("execute_async_v3 failed")
            cuda.event_record(stop_event, stream)
            cuda.event_synchronize(stop_event)
            samples.append(cuda.event_elapsed_ms(start_event, stop_event))
            if not args.enqueue_emits_profile:
                ok = context.report_to_profiler()
                if ok is False:
                    raise RuntimeError("report_to_profiler returned false")
        cuda.stream_synchronize(stream)
        wall_ms = (time.perf_counter() - wall_started) * 1000.0
    finally:
        for ptr in buffers:
            cuda.free(ptr)
        cuda.event_destroy(start_event)
        cuda.event_destroy(stop_event)
        cuda.stream_destroy(stream)

    mean_ms = statistics.fmean(samples) if samples else 0.0
    layer_report = summarize_layers(profiler.records, args.keep_layers)
    result = {
        "engine": str(engine_path.resolve()),
        "engine_bytes": len(engine_bytes),
        "load_ms": load_ms,
        "warmup": args.warmup,
        "iterations": args.iterations,
        "input_batch": input_batch,
        "tensors": tensors,
        "enqueue_emits_profile": args.enqueue_emits_profile,
        "event_mean_ms": mean_ms,
        "event_median_ms": statistics.median(samples) if samples else 0.0,
        "event_min_ms": min(samples) if samples else 0.0,
        "event_max_ms": max(samples) if samples else 0.0,
        "event_p90_ms": percentile(samples, 0.90),
        "event_p99_ms": percentile(samples, 0.99),
        "wall_ms": wall_ms,
        "event_images_per_second": (input_batch * 1000.0 / mean_ms) if mean_ms else 0.0,
        "wall_images_per_second": (input_batch * args.iterations * 1000.0 / wall_ms)
        if wall_ms
        else 0.0,
        "profile_records": len(profiler.records),
        "layer_total_ms": layer_report["total_ms"],
        "unique_layers": layer_report["unique_layers"],
        "layer_categories": layer_report["categories"],
        "top_layers": layer_report["top_layers"],
        "samples_ms": samples[: args.keep_samples],
    }
    if args.inspect:
        result["engine_inspection"] = engine_inspection(trt, engine, context)
    return result


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--engine", required=True, type=Path)
    parser.add_argument("--warmup", type=int, default=10)
    parser.add_argument("--iterations", type=int, default=20)
    parser.add_argument("--keep-samples", type=int, default=20)
    parser.add_argument("--keep-layers", type=int, default=40)
    parser.add_argument("--enqueue-emits-profile", action="store_true")
    parser.add_argument("--inspect", action="store_true")
    parser.add_argument("--out", type=Path)
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    report = profile(args)
    text = json.dumps(report, ensure_ascii=False, indent=2)
    if args.out:
        args.out.parent.mkdir(parents=True, exist_ok=True)
        args.out.write_text(text + "\n", encoding="utf-8")
    print(text)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
