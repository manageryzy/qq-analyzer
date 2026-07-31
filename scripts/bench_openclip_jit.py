#!/usr/bin/env python3
"""Benchmark native OpenCLIP MobileCLIP image encoding with Torch JIT."""

from __future__ import annotations

import argparse
import json
import time
from pathlib import Path

import open_clip
import torch


class EncodeImage(torch.nn.Module):
    def __init__(self, model: torch.nn.Module) -> None:
        super().__init__()
        self.model = model

    def forward(self, image: torch.Tensor) -> torch.Tensor:
        return self.model.encode_image(image, normalize=True)


def dtype_from_precision(precision: str) -> torch.dtype:
    if precision in {"fp16", "amp"}:
        return torch.float16
    return torch.float32


def bench(module: torch.nn.Module, x: torch.Tensor, warmup: int, iters: int) -> dict[str, float]:
    times: list[float] = []
    with torch.inference_mode():
        for _ in range(warmup):
            module(x)
        torch.cuda.synchronize()
        wall_start = time.perf_counter()
        for _ in range(iters):
            start = torch.cuda.Event(enable_timing=True)
            end = torch.cuda.Event(enable_timing=True)
            start.record()
            module(x)
            end.record()
            torch.cuda.synchronize()
            times.append(float(start.elapsed_time(end)))
        wall_ms = (time.perf_counter() - wall_start) * 1000.0
    t = torch.tensor(times, dtype=torch.float64)
    images = int(x.shape[0]) * iters
    total_ms = float(t.sum().item())
    return {
        "iters": iters,
        "batch_size": int(x.shape[0]),
        "event_total_ms": total_ms,
        "event_mean_ms": float(t.mean().item()),
        "event_median_ms": float(t.median().item()),
        "event_min_ms": float(t.min().item()),
        "event_max_ms": float(t.max().item()),
        "wall_ms": wall_ms,
        "event_images_per_second": images / (total_ms / 1000.0),
        "wall_images_per_second": images / (wall_ms / 1000.0),
    }


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--model", default="MobileCLIP2-S2")
    parser.add_argument("--pretrained", default="dfndr2b")
    parser.add_argument("--precision", choices=["fp32", "fp16", "amp"], default="fp16")
    parser.add_argument("--batch-size", type=int, default=128)
    parser.add_argument("--image-size", type=int, default=256)
    parser.add_argument("--warmup", type=int, default=10)
    parser.add_argument("--iters", type=int, default=30)
    parser.add_argument("--skip-freeze", action="store_true")
    parser.add_argument(
        "--cache-dir", type=Path, default=Path.home() / ".cache" / "open_clip"
    )
    parser.add_argument("--out", type=Path, default=None)
    parser.add_argument("--channels-last", action="store_true")
    parser.add_argument("--compile-mode", choices=["none", "default", "reduce-overhead", "max-autotune"], default="none")
    parser.add_argument("--skip-eager", action="store_true")
    parser.add_argument("--skip-jit", action="store_true")
    args = parser.parse_args()

    if not torch.cuda.is_available():
        raise SystemExit("torch cuda is not available")
    if args.batch_size <= 0:
        raise SystemExit("--batch-size must be positive")

    args.cache_dir.mkdir(parents=True, exist_ok=True)
    load_started = time.perf_counter()
    model, _ = open_clip.create_model_from_pretrained(
        args.model,
        pretrained=args.pretrained,
        precision=args.precision,
        device="cuda",
        cache_dir=str(args.cache_dir),
    )
    model.eval()
    wrapped = EncodeImage(model).eval().cuda()
    if args.channels_last:
        wrapped = wrapped.to(memory_format=torch.channels_last)
    load_ms = (time.perf_counter() - load_started) * 1000.0

    dtype = dtype_from_precision(args.precision)
    x = torch.randn(
        args.batch_size,
        3,
        args.image_size,
        args.image_size,
        device="cuda",
        dtype=dtype,
    )
    if args.channels_last:
        x = x.contiguous(memory_format=torch.channels_last)
    report: dict[str, object] = {
        "model": args.model,
        "pretrained": args.pretrained,
        "precision": args.precision,
        "torch_version": torch.__version__,
        "cuda_version": torch.version.cuda,
        "device": torch.cuda.get_device_name(0),
        "input_shape": list(x.shape),
        "input_dtype": str(x.dtype).replace("torch.", ""),
        "channels_last": args.channels_last,
        "compile_mode": args.compile_mode,
        "load_ms": load_ms,
    }
    if not args.skip_eager:
        report["eager"] = bench(wrapped, x, args.warmup, args.iters)

    if args.compile_mode != "none":
        compile_started = time.perf_counter()
        mode = None if args.compile_mode == "default" else args.compile_mode
        compiled = torch.compile(wrapped, mode=mode, fullgraph=False)
        torch.cuda.synchronize()
        report["compile_ms"] = (time.perf_counter() - compile_started) * 1000.0
        report["compiled"] = bench(compiled, x, args.warmup, args.iters)

    if not args.skip_jit:
        trace_started = time.perf_counter()
        with torch.inference_mode():
            traced = torch.jit.trace(wrapped, x, strict=False).eval()
            if args.skip_freeze:
                jit_module = traced
                freeze_error = "skipped"
                jit_label = "jit_traced"
            else:
                try:
                    jit_module = torch.jit.freeze(traced)
                    freeze_error = None
                    jit_label = "jit_frozen"
                except Exception as err:
                    jit_module = traced
                    freeze_error = repr(err)
                    jit_label = "jit_traced"
        torch.cuda.synchronize()
        report["trace_freeze_ms"] = (time.perf_counter() - trace_started) * 1000.0
        report["jit_freeze_error"] = freeze_error
        report[jit_label] = bench(jit_module, x, args.warmup, args.iters)

    text = json.dumps(report, indent=2)
    if args.out is not None:
        args.out.parent.mkdir(parents=True, exist_ok=True)
        args.out.write_text(text, encoding="utf-8")
    print(text)


if __name__ == "__main__":
    main()
