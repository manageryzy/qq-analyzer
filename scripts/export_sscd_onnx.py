#!/usr/bin/env python3
"""Export the official SSCD TorchScript model to ONNX for Rust/ORT indexing."""

from __future__ import annotations

import argparse
from pathlib import Path


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--input", required=True, type=Path, help="SSCD .torchscript.pt file")
    parser.add_argument("--output", required=True, type=Path, help="Output .onnx path")
    parser.add_argument("--opset", default=17, type=int)
    parser.add_argument("--batch", default=2, type=int, help="Validation batch size")
    return parser.parse_args()


def main() -> None:
    args = parse_args()
    args.output.parent.mkdir(parents=True, exist_ok=True)

    import numpy as np
    import torch

    model = torch.jit.load(str(args.input), map_location="cpu")
    model.eval()
    dummy = torch.randn(args.batch, 3, 320, 320, dtype=torch.float32)

    with torch.no_grad():
        reference = model(dummy).detach().cpu().numpy()

    torch.onnx.export(
        model,
        dummy,
        str(args.output),
        input_names=["input"],
        output_names=["embedding"],
        dynamic_axes={"input": {0: "batch"}, "embedding": {0: "batch"}},
        opset_version=args.opset,
        do_constant_folding=True,
        dynamo=False,
    )

    import onnx

    onnx_model = onnx.load(str(args.output))
    onnx.checker.check_model(onnx_model)

    try:
        import onnxruntime as ort
    except ImportError:
        print(f"exported={args.output}")
        print(f"reference_shape={reference.shape}")
        print("onnxruntime=unavailable")
        return

    session = ort.InferenceSession(str(args.output), providers=["CPUExecutionProvider"])
    actual = session.run(["embedding"], {"input": dummy.numpy()})[0]
    max_abs_diff = float(np.max(np.abs(actual - reference)))
    norms = np.linalg.norm(actual, axis=1)
    print(f"exported={args.output}")
    print(f"reference_shape={reference.shape}")
    print(f"onnx_shape={actual.shape}")
    print(f"max_abs_diff={max_abs_diff:.8f}")
    print(f"norm_min={float(norms.min()):.8f}")
    print(f"norm_max={float(norms.max()):.8f}")
    if actual.shape != reference.shape:
        raise SystemExit("ONNX output shape does not match TorchScript output shape")
    if max_abs_diff > 1e-4:
        raise SystemExit(f"ONNX output differs from TorchScript by {max_abs_diff}")


if __name__ == "__main__":
    main()
