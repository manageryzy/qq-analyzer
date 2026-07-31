from __future__ import annotations

import argparse
import json
import os
import sys
from pathlib import Path
from typing import Any


def load_tensorrt(repo: Path) -> Any:
    trt_py = repo / "output" / "_deps" / "tensorrt-cu12-py"
    if trt_py.is_dir():
        sys.path.insert(0, str(trt_py))
    if os.name == "nt":
        trt_libs = trt_py / "tensorrt_libs"
        if trt_libs.is_dir():
            os.add_dll_directory(str(trt_libs))
    import tensorrt as trt

    return trt


def parse_network(trt: Any, onnx_path: Path) -> tuple[Any, Any, list[str]]:
    logger = trt.Logger(trt.Logger.ERROR)
    builder = trt.Builder(logger)
    flags = 1 << int(trt.NetworkDefinitionCreationFlag.EXPLICIT_BATCH)
    network = builder.create_network(flags)
    parser = trt.OnnxParser(network, logger)
    previous_cwd = Path.cwd()
    try:
        os.chdir(onnx_path.parent)
        ok = parser.parse_from_file(onnx_path.name)
    finally:
        os.chdir(previous_cwd)
    errors = [str(parser.get_error(index)) for index in range(parser.num_errors)]
    if not ok:
        raise RuntimeError("TensorRT ONNX parse failed:\n" + "\n".join(errors))
    return builder, network, errors


def tensor_info(tensor: Any) -> dict[str, Any]:
    return {
        "name": getattr(tensor, "name", None),
        "dtype": str(getattr(tensor, "dtype", None)),
        "shape": [int(x) for x in getattr(tensor, "shape", [])],
    }


def layer_info(layer: Any) -> dict[str, Any]:
    row: dict[str, Any] = {
        "name": layer.name,
        "type": str(layer.type),
        "precision": str(getattr(layer, "precision", None)),
        "precision_is_set": bool(getattr(layer, "precision_is_set", False)),
        "num_inputs": int(getattr(layer, "num_inputs", 0)),
        "num_outputs": int(getattr(layer, "num_outputs", 0)),
    }
    for attr in ("num_groups", "num_output_maps", "kernel_size", "stride", "padding", "dilation"):
        if hasattr(layer, attr):
            value = getattr(layer, attr)
            try:
                row[attr] = [int(x) for x in value]
            except TypeError:
                row[attr] = int(value)
    row["inputs"] = [tensor_info(layer.get_input(index)) for index in range(row["num_inputs"])]
    row["outputs"] = [tensor_info(layer.get_output(index)) for index in range(row["num_outputs"])]
    return row


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--onnx", required=True, type=Path)
    parser.add_argument("--name", action="append", default=[])
    parser.add_argument("--out", type=Path)
    args = parser.parse_args()

    repo = Path(__file__).resolve().parents[1]
    trt = load_tensorrt(repo)
    _, network, errors = parse_network(trt, args.onnx.resolve())
    wanted = set(args.name)
    layers = []
    for index in range(network.num_layers):
        layer = network.get_layer(index)
        if not wanted or layer.name in wanted:
            layers.append(layer_info(layer))
    result = {
        "onnx": str(args.onnx.resolve()),
        "num_layers": network.num_layers,
        "parse_errors": errors,
        "layers": layers,
    }
    text = json.dumps(result, ensure_ascii=False, indent=2)
    if args.out:
        args.out.parent.mkdir(parents=True, exist_ok=True)
        args.out.write_text(text + "\n", encoding="utf-8")
    print(text)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
