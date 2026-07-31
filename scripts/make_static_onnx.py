#!/usr/bin/env python3
"""Rewrite an ONNX model to expose a static input shape with a front Reshape.

This is intended for profiling ORT/TensorRT behavior with a fixed batch size.
It does not change weight values. The external graph input keeps the original
name so existing Rust code can keep feeding the model normally; all original
consumers are rewired to the new static reshape output.
"""

from __future__ import annotations

import argparse
import shutil
from pathlib import Path

import numpy as np
import onnx
from onnx import TensorProto, helper, numpy_helper, shape_inference


def parse_shape(value: str) -> list[int]:
    parts = [part.strip() for part in value.replace("x", ",").split(",")]
    shape: list[int] = []
    for part in parts:
        if not part:
            continue
        dim = int(part)
        if dim <= 0:
            raise argparse.ArgumentTypeError(f"shape dimensions must be positive: {value}")
        shape.append(dim)
    if not shape:
        raise argparse.ArgumentTypeError("shape must not be empty")
    return shape


def tensor_shape(value_info: onnx.ValueInfoProto) -> list[int | str | None]:
    dims: list[int | str | None] = []
    tensor_type = value_info.type.tensor_type
    for dim in tensor_type.shape.dim:
        if dim.HasField("dim_value"):
            dims.append(dim.dim_value)
        elif dim.HasField("dim_param"):
            dims.append(dim.dim_param)
        else:
            dims.append(None)
    return dims


def set_tensor_shape(value_info: onnx.ValueInfoProto, shape: list[int]) -> None:
    dims = value_info.type.tensor_type.shape.dim
    del dims[:]
    for value in shape:
        dims.add().dim_value = int(value)


def infer_static_shape(model: onnx.ModelProto, input_name: str, batch_size: int | None) -> list[int]:
    for value_info in model.graph.input:
        if value_info.name != input_name:
            continue
        dims = tensor_shape(value_info)
        if not dims:
            raise SystemExit(f"input {input_name!r} has no tensor shape; pass --shape")
        out: list[int] = []
        for index, dim in enumerate(dims):
            if index == 0 and batch_size is not None:
                out.append(batch_size)
            elif isinstance(dim, int) and dim > 0:
                out.append(dim)
            else:
                raise SystemExit(
                    f"input {input_name!r} has non-static dimension {index}={dim!r}; "
                    "pass --shape"
                )
        return out
    raise SystemExit(f"input {input_name!r} not found")


def first_real_input(model: onnx.ModelProto) -> str:
    initializer_names = {initializer.name for initializer in model.graph.initializer}
    for value_info in model.graph.input:
        if value_info.name not in initializer_names:
            return value_info.name
    raise SystemExit("model has no non-initializer graph input")


def remove_existing_front_reshape(model: onnx.ModelProto, input_name: str) -> None:
    marker = f"{input_name}_front_static_"
    if not model.graph.node:
        return
    first = model.graph.node[0]
    if first.op_type != "Reshape" or not first.name.startswith(marker):
        return
    if len(first.input) < 1 or first.input[0] != input_name or len(first.output) != 1:
        return
    shape_input = first.input[1] if len(first.input) > 1 else ""
    reshape_output = first.output[0]
    del model.graph.node[0]
    for node in model.graph.node:
        for index, name in enumerate(node.input):
            if name == reshape_output:
                node.input[index] = input_name
    keep = [value_info for value_info in model.graph.value_info if value_info.name != reshape_output]
    del model.graph.value_info[:]
    model.graph.value_info.extend(keep)
    if shape_input:
        keep_initializers = [
            initializer for initializer in model.graph.initializer if initializer.name != shape_input
        ]
        del model.graph.initializer[:]
        model.graph.initializer.extend(keep_initializers)


def rewrite_model(
    model: onnx.ModelProto,
    input_name: str,
    static_shape: list[int],
    output_batch: bool,
    add_reshape: bool,
) -> onnx.ModelProto:
    remove_existing_front_reshape(model, input_name)

    input_info = None
    for value_info in model.graph.input:
        if value_info.name == input_name:
            input_info = value_info
            break
    if input_info is None:
        raise SystemExit(f"input {input_name!r} not found")
    set_tensor_shape(input_info, static_shape)

    if output_batch:
        for output in model.graph.output:
            dims = tensor_shape(output)
            if dims:
                new_dims: list[int] = []
                for index, dim in enumerate(dims):
                    if index == 0:
                        new_dims.append(static_shape[0])
                    elif isinstance(dim, int) and dim > 0:
                        new_dims.append(dim)
                    else:
                        break
                if len(new_dims) == len(dims):
                    set_tensor_shape(output, new_dims)

    if add_reshape:
        suffix = "x".join(str(dim) for dim in static_shape)
        shape_name = f"{input_name}_front_static_{suffix}_shape"
        reshape_output = f"{input_name}_front_static_{suffix}"
        shape_initializer = numpy_helper.from_array(
            np.array(static_shape, dtype="int64"),
            name=shape_name,
        )
        model.graph.initializer.append(shape_initializer)

        for node in model.graph.node:
            for index, name in enumerate(node.input):
                if name == input_name:
                    node.input[index] = reshape_output

        elem_type = input_info.type.tensor_type.elem_type or TensorProto.FLOAT
        reshape_info = helper.make_tensor_value_info(reshape_output, elem_type, static_shape)
        model.graph.value_info.append(reshape_info)
        reshape_node = helper.make_node(
            "Reshape",
            [input_name, shape_name],
            [reshape_output],
            name=f"{input_name}_front_static_{suffix}_Reshape",
        )
        model.graph.node.insert(0, reshape_node)

    return model


def save_model(model: onnx.ModelProto, output: Path, external_data_threshold: int) -> None:
    output.parent.mkdir(parents=True, exist_ok=True)
    data_file = output.name + ".data"
    onnx.save_model(
        model,
        str(output),
        save_as_external_data=True,
        all_tensors_to_one_file=True,
        location=data_file,
        size_threshold=external_data_threshold,
        convert_attribute=False,
    )


def copy_sibling_files(source: Path, output: Path) -> None:
    allowed = {
        "open_clip_config.json",
        "model_config.json",
        "tokenizer.json",
        "tokenizer_config.json",
        "special_tokens_map.json",
        "text.onnx",
        "text.onnx.data",
    }
    for name in allowed:
        src = source.parent / name
        if src.is_file():
            shutil.copy2(src, output.parent / name)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--input", required=True, type=Path, help="Input ONNX model")
    parser.add_argument("--output", required=True, type=Path, help="Output ONNX model")
    parser.add_argument("--input-name", default="", help="Graph input to rewrite")
    parser.add_argument("--batch-size", type=int, default=None, help="Static batch size")
    parser.add_argument("--shape", type=parse_shape, default=None, help="Full static shape, e.g. 128,3,256,256")
    parser.add_argument("--no-front-reshape", action="store_true", help="Only set static graph input/output shapes")
    parser.add_argument("--no-output-batch", action="store_true", help="Do not rewrite output dim 0")
    parser.add_argument("--no-shape-inference", action="store_true", help="Skip ONNX shape inference")
    parser.add_argument("--copy-siblings", action="store_true", help="Copy CLIP config/text/tokenizer siblings")
    parser.add_argument(
        "--external-data-threshold",
        type=int,
        default=1024,
        help="Tensor byte threshold for external data when saving",
    )
    args = parser.parse_args()

    if args.batch_size is not None and args.batch_size <= 0:
        raise SystemExit("--batch-size must be positive")
    if not args.input.is_file():
        raise SystemExit(f"input does not exist: {args.input}")

    model = onnx.load(str(args.input), load_external_data=True)
    input_name = args.input_name or first_real_input(model)
    static_shape = args.shape or infer_static_shape(model, input_name, args.batch_size)
    if args.batch_size is not None and args.shape is not None:
        static_shape[0] = args.batch_size

    model = rewrite_model(
        model,
        input_name=input_name,
        static_shape=static_shape,
        output_batch=not args.no_output_batch,
        add_reshape=not args.no_front_reshape,
    )
    if not args.no_shape_inference:
        model = shape_inference.infer_shapes(model)
    onnx.checker.check_model(model)
    save_model(model, args.output, args.external_data_threshold)
    if args.copy_siblings:
        copy_sibling_files(args.input, args.output)

    print(f"input={args.input}")
    print(f"output={args.output}")
    print(f"input_name={input_name}")
    print(f"static_shape={','.join(str(dim) for dim in static_shape)}")
    print(f"front_reshape={not args.no_front_reshape}")


if __name__ == "__main__":
    main()
