#!/usr/bin/env python3
"""Rewrite selected ONNX graph inputs to FLOAT16 without changing weights."""

from __future__ import annotations

import argparse
import shutil
from pathlib import Path

import onnx
from onnx import TensorProto


def set_input_fp16(model: onnx.ModelProto, input_names: set[str]) -> list[str]:
    changed: list[str] = []
    for value_info in model.graph.input:
        if input_names and value_info.name not in input_names:
            continue
        tensor_type = value_info.type.tensor_type
        if tensor_type.elem_type != TensorProto.FLOAT16:
            tensor_type.elem_type = TensorProto.FLOAT16
            changed.append(value_info.name)
    return changed


def copy_siblings(source: Path, output: Path) -> None:
    allowed = {
        "open_clip_config.json",
        "model_config.json",
        "tokenizer.json",
        "tokenizer_config.json",
        "special_tokens_map.json",
        "text.onnx",
        "text.onnx.data",
    }
    output.parent.mkdir(parents=True, exist_ok=True)
    for name in allowed:
        src = source.parent / name
        if src.is_file():
            shutil.copy2(src, output.parent / name)


def save_model(model: onnx.ModelProto, output: Path, threshold: int) -> None:
    output.parent.mkdir(parents=True, exist_ok=True)
    onnx.save_model(
        model,
        str(output),
        save_as_external_data=True,
        all_tensors_to_one_file=True,
        location=output.name + ".data",
        size_threshold=threshold,
        convert_attribute=False,
    )


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--input", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--input-name", action="append", default=[])
    parser.add_argument("--copy-siblings", action="store_true")
    parser.add_argument("--external-data-threshold", type=int, default=1024)
    args = parser.parse_args()

    if not args.input.is_file():
        raise SystemExit(f"input does not exist: {args.input}")

    model = onnx.load(str(args.input), load_external_data=True)
    changed = set_input_fp16(model, set(args.input_name))
    if not changed:
        raise SystemExit("no graph inputs were changed")
    onnx.checker.check_model(model)
    save_model(model, args.output, args.external_data_threshold)
    if args.copy_siblings:
        copy_siblings(args.input, args.output)

    print(f"input={args.input}")
    print(f"output={args.output}")
    print(f"changed_inputs={','.join(changed)}")


if __name__ == "__main__":
    main()
