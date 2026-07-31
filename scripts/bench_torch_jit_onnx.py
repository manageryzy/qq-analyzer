#!/usr/bin/env python3
"""Benchmark a Torch/JIT module converted from an ONNX model.

This is an apples-to-apples GPU forward benchmark for the MobileCLIP visual
graph. It uses random input tensors and excludes image decode, SQLite writes,
and Rust pipeline scheduling.
"""

from __future__ import annotations

import argparse
import json
import time
from pathlib import Path

import onnx
import torch
from onnx import helper, numpy_helper
from onnx2torch import convert


def parse_shape(value: str) -> tuple[int, ...]:
    dims = tuple(int(part) for part in value.replace("x", ",").split(",") if part)
    if not dims or any(dim <= 0 for dim in dims):
        raise argparse.ArgumentTypeError(f"invalid shape: {value}")
    return dims


def dtype_from_name(name: str) -> torch.dtype:
    if name == "float32":
        return torch.float32
    if name == "float16":
        return torch.float16
    raise argparse.ArgumentTypeError(f"unsupported dtype: {name}")


def benchmark_module(module: torch.nn.Module, input_tensor: torch.Tensor, warmup: int, iters: int) -> dict[str, float]:
    times_ms: list[float] = []
    with torch.inference_mode():
        for _ in range(warmup):
            output = module(input_tensor)
            if isinstance(output, (tuple, list)):
                output = output[0]
        torch.cuda.synchronize()
        wall_start = time.perf_counter()
        for _ in range(iters):
            start = torch.cuda.Event(enable_timing=True)
            end = torch.cuda.Event(enable_timing=True)
            start.record()
            output = module(input_tensor)
            if isinstance(output, (tuple, list)):
                output = output[0]
            end.record()
            torch.cuda.synchronize()
            times_ms.append(float(start.elapsed_time(end)))
        wall_ms = (time.perf_counter() - wall_start) * 1000.0
    times = torch.tensor(times_ms, dtype=torch.float64)
    images = input_tensor.shape[0] * iters
    return {
        "iters": iters,
        "batch_size": input_tensor.shape[0],
        "event_total_ms": float(times.sum().item()),
        "event_mean_ms": float(times.mean().item()),
        "event_median_ms": float(times.median().item()),
        "event_min_ms": float(times.min().item()),
        "event_max_ms": float(times.max().item()),
        "wall_ms": wall_ms,
        "event_images_per_second": images / (float(times.sum().item()) / 1000.0),
        "wall_images_per_second": images / (wall_ms / 1000.0),
    }


def adapt_for_onnx2torch(model: onnx.ModelProto) -> dict[str, int]:
    """Downgrade simple opset-18 ReduceMean nodes for onnx2torch."""
    initializers = {initializer.name: initializer for initializer in model.graph.initializer}
    converted_reduce_axes = 0
    removed_reshape_allowzero = 0
    for node in model.graph.node:
        if not node.op_type.startswith("Reduce") or len(node.input) != 2:
            continue
        axes_initializer = initializers.get(node.input[1])
        if axes_initializer is None:
            continue
        axes = numpy_helper.to_array(axes_initializer).astype("int64").reshape(-1).tolist()
        data_input = node.input[0]
        del node.input[:]
        node.input.append(data_input)
        keep_attrs = [
            attr
            for attr in node.attribute
            if attr.name not in {"axes", "noop_with_empty_axes"}
        ]
        del node.attribute[:]
        node.attribute.extend(keep_attrs)
        node.attribute.append(helper.make_attribute("axes", [int(axis) for axis in axes]))
        converted_reduce_axes += 1

    for node in model.graph.node:
        if node.op_type != "Reshape":
            continue
        attrs = [attr for attr in node.attribute if attr.name != "allowzero"]
        if len(attrs) != len(node.attribute):
            del node.attribute[:]
            node.attribute.extend(attrs)
            removed_reshape_allowzero += 1

    if converted_reduce_axes:
        for opset in model.opset_import:
            if opset.domain == "" and opset.version > 17:
                opset.version = 17
    return {
        "converted_reduce_axes": converted_reduce_axes,
        "removed_reshape_allowzero": removed_reshape_allowzero,
    }


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--model", required=True, type=Path)
    parser.add_argument("--batch-size", type=int, default=128)
    parser.add_argument("--shape", type=parse_shape, default=None)
    parser.add_argument("--dtype", choices=["float32", "float16"], default="float32")
    parser.add_argument("--warmup", type=int, default=10)
    parser.add_argument("--iters", type=int, default=30)
    parser.add_argument("--out", type=Path, default=None)
    args = parser.parse_args()

    if not args.model.is_file():
        raise SystemExit(f"model not found: {args.model}")
    if args.batch_size <= 0:
        raise SystemExit("--batch-size must be positive")
    if not torch.cuda.is_available():
        raise SystemExit("torch cuda is not available")

    shape = args.shape or (args.batch_size, 3, 256, 256)
    if shape[0] != args.batch_size:
        raise SystemExit("--shape batch dimension must match --batch-size")
    dtype = dtype_from_name(args.dtype)

    load_started = time.perf_counter()
    onnx_model = onnx.load(str(args.model), load_external_data=True)
    report_adaptations = adapt_for_onnx2torch(onnx_model)
    torch_module = convert(onnx_model).eval().cuda()
    load_ms = (time.perf_counter() - load_started) * 1000.0

    input_tensor = torch.randn(shape, device="cuda", dtype=dtype)
    report: dict[str, object] = {
        "model": str(args.model),
        "torch_version": torch.__version__,
        "cuda_version": torch.version.cuda,
        "device": torch.cuda.get_device_name(0),
        "input_shape": list(shape),
        "input_dtype": str(dtype).replace("torch.", ""),
        "load_convert_ms": load_ms,
        "onnx2torch_adaptations": report_adaptations,
    }

    report["eager"] = benchmark_module(torch_module, input_tensor, args.warmup, args.iters)

    trace_started = time.perf_counter()
    with torch.inference_mode():
        traced = torch.jit.trace(torch_module, input_tensor, strict=False).eval()
        frozen = torch.jit.freeze(traced)
    torch.cuda.synchronize()
    report["trace_freeze_ms"] = (time.perf_counter() - trace_started) * 1000.0
    report["jit_frozen"] = benchmark_module(frozen, input_tensor, args.warmup, args.iters)

    text = json.dumps(report, indent=2)
    if args.out is not None:
        args.out.parent.mkdir(parents=True, exist_ok=True)
        args.out.write_text(text, encoding="utf-8")
    print(text)


if __name__ == "__main__":
    main()
