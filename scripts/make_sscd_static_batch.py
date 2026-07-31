#!/usr/bin/env python3
"""Create a fixed-batch SSCD ONNX model from an existing static template."""

from __future__ import annotations

import argparse
from pathlib import Path


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--input", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--batch-size", required=True, type=int)
    return parser.parse_args()


def main() -> None:
    args = parse_args()
    if args.batch_size <= 0:
        raise SystemExit("--batch-size must be positive")

    import numpy as np
    import onnx
    from onnx import numpy_helper

    model = onnx.load(str(args.input), load_external_data=True)
    if len(model.graph.input) != 1 or len(model.graph.output) != 1:
        raise SystemExit("expected one SSCD input and one output")

    input_shape = model.graph.input[0].type.tensor_type.shape.dim
    output_shape = model.graph.output[0].type.tensor_type.shape.dim
    if len(input_shape) != 4 or len(output_shape) != 2:
        raise SystemExit("expected SSCD input [batch,3,320,320] and output [batch,dim]")
    source_batch_size = input_shape[0].dim_value
    if source_batch_size <= 0:
        raise SystemExit("input template must already have a fixed batch size")
    input_shape[0].ClearField("dim_param")
    input_shape[0].dim_value = args.batch_size
    output_shape[0].ClearField("dim_param")
    output_shape[0].dim_value = args.batch_size
    for value_info in model.graph.value_info:
        tensor_type = value_info.type.tensor_type
        if not tensor_type.HasField("shape") or not tensor_type.shape.dim:
            continue
        batch_dim = tensor_type.shape.dim[0]
        if batch_dim.HasField("dim_value") and batch_dim.dim_value == source_batch_size:
            batch_dim.dim_value = args.batch_size

    front_reshape = None
    for initializer in model.graph.initializer:
        values = numpy_helper.to_array(initializer)
        if values.shape == (4,) and tuple(int(value) for value in values[1:]) == (3, 320, 320):
            front_reshape = initializer
            replacement = numpy_helper.from_array(
                np.asarray([args.batch_size, 3, 320, 320], dtype=np.int64),
                initializer.name,
            )
            initializer.CopyFrom(replacement)
            break
    if front_reshape is None:
        raise SystemExit("could not find the SSCD front reshape initializer")

    onnx.checker.check_model(model)
    args.output.parent.mkdir(parents=True, exist_ok=True)
    external_name = args.output.name + ".data"
    args.output.unlink(missing_ok=True)
    args.output.with_name(external_name).unlink(missing_ok=True)
    onnx.save_model(
        model,
        str(args.output),
        save_as_external_data=True,
        all_tensors_to_one_file=True,
        location=external_name,
        size_threshold=1024,
        convert_attribute=False,
    )
    onnx.checker.check_model(str(args.output))
    print(f"output={args.output}")
    print(f"batch_size={args.batch_size}")
    print(f"external_data={args.output.with_name(external_name)}")


if __name__ == "__main__":
    main()
