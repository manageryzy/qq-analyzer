#!/usr/bin/env python3
"""Fuse decomposed exact GELU subgraphs in an ONNX model.

The MobileCLIP visual export currently represents GELU as:

    x / sqrt(2) -> Erf -> + 1 -> * 0.5 -> * x

This rewrites that pattern to com.microsoft::Gelu so ONNX Runtime can dispatch a
single fused contrib op instead of several large elementwise kernels.
"""

from __future__ import annotations

import argparse
import shutil
from collections import defaultdict
from pathlib import Path

import numpy as np
import onnx
from onnx import helper, numpy_helper


def scalar_initializer(initializers: dict[str, onnx.TensorProto], name: str) -> float | None:
    value = initializers.get(name)
    if value is None:
        return None
    array = numpy_helper.to_array(value)
    if array.shape not in [(), (1,)]:
        return None
    return float(array.reshape(-1)[0])


def input_matching_scalar(
    node: onnx.NodeProto,
    initializers: dict[str, onnx.TensorProto],
    expected: float,
    *,
    atol: float,
) -> str | None:
    non_const_inputs: list[str] = []
    matched = False
    for name in node.input:
        value = scalar_initializer(initializers, name)
        if value is None:
            non_const_inputs.append(name)
            continue
        if np.isclose(value, expected, atol=atol, rtol=0.0):
            matched = True
    if matched and len(non_const_inputs) == 1:
        return non_const_inputs[0]
    return None


def ensure_opset(model: onnx.ModelProto, domain: str, version: int) -> None:
    for opset in model.opset_import:
        if opset.domain == domain:
            if opset.version < version:
                opset.version = version
            return
    model.opset_import.append(helper.make_operatorsetid(domain, version))


def remove_unused_initializers(model: onnx.ModelProto) -> int:
    used = {name for node in model.graph.node for name in node.input}
    used.update(output.name for output in model.graph.output)
    kept = [initializer for initializer in model.graph.initializer if initializer.name in used]
    removed = len(model.graph.initializer) - len(kept)
    del model.graph.initializer[:]
    model.graph.initializer.extend(kept)
    return removed


def fuse_exact_gelu(model: onnx.ModelProto, *, atol: float) -> tuple[int, int]:
    nodes = list(model.graph.node)
    producers = {output: node for node in nodes for output in node.output}
    consumers: dict[str, list[onnx.NodeProto]] = defaultdict(list)
    for node in nodes:
        for input_name in node.input:
            consumers[input_name].append(node)

    initializers = {initializer.name: initializer for initializer in model.graph.initializer}
    remove_ids: set[int] = set()
    replacement_by_final_id: dict[int, onnx.NodeProto] = {}

    for div in nodes:
        if div.op_type != "Div" or len(div.output) != 1:
            continue
        x = input_matching_scalar(div, initializers, np.sqrt(2.0), atol=atol)
        if x is None:
            continue
        div_users = consumers.get(div.output[0], [])
        if len(div_users) != 1 or div_users[0].op_type != "Erf":
            continue
        erf = div_users[0]
        if len(erf.output) != 1:
            continue
        erf_users = consumers.get(erf.output[0], [])
        if len(erf_users) != 1 or erf_users[0].op_type != "Add":
            continue
        add = erf_users[0]
        add_payload = input_matching_scalar(add, initializers, 1.0, atol=atol)
        if add_payload != erf.output[0] or len(add.output) != 1:
            continue
        add_users = consumers.get(add.output[0], [])
        if len(add_users) != 1 or add_users[0].op_type != "Mul":
            continue
        half_mul = add_users[0]
        half_payload = input_matching_scalar(half_mul, initializers, 0.5, atol=atol)
        if half_payload != add.output[0] or len(half_mul.output) != 1:
            continue
        half_users = consumers.get(half_mul.output[0], [])
        if len(half_users) != 1 or half_users[0].op_type != "Mul":
            continue
        final_mul = half_users[0]
        if len(final_mul.output) != 1:
            continue
        if sorted(final_mul.input) != sorted([x, half_mul.output[0]]):
            continue

        chain = [div, erf, add, half_mul, final_mul]
        if any(id(node) in remove_ids for node in chain):
            continue
        fused = helper.make_node(
            "Gelu",
            [x],
            [final_mul.output[0]],
            name=(final_mul.name or final_mul.output[0]) + "_com_microsoft_gelu",
            domain="com.microsoft",
        )
        for node in chain:
            remove_ids.add(id(node))
        replacement_by_final_id[id(final_mul)] = fused

    if not replacement_by_final_id:
        return 0, 0

    fused_nodes: list[onnx.NodeProto] = []
    for node in nodes:
        replacement = replacement_by_final_id.get(id(node))
        if replacement is not None:
            fused_nodes.append(replacement)
            continue
        if id(node) in remove_ids:
            continue
        fused_nodes.append(node)

    del model.graph.node[:]
    model.graph.node.extend(fused_nodes)
    ensure_opset(model, "com.microsoft", 1)
    removed_initializers = remove_unused_initializers(model)
    return len(replacement_by_final_id), removed_initializers


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
    parser.add_argument("--copy-siblings", action="store_true")
    parser.add_argument("--atol", type=float, default=1e-3)
    parser.add_argument("--external-data-threshold", type=int, default=1024)
    args = parser.parse_args()

    if not args.input.is_file():
        raise SystemExit(f"input does not exist: {args.input}")

    model = onnx.load(str(args.input), load_external_data=True)
    before_nodes = len(model.graph.node)
    fused, removed_initializers = fuse_exact_gelu(model, atol=args.atol)
    if fused == 0:
        raise SystemExit("no exact GELU subgraphs were fused")
    onnx.checker.check_model(model)
    save_model(model, args.output, args.external_data_threshold)
    if args.copy_siblings:
        copy_siblings(args.input, args.output)

    print(f"input={args.input}")
    print(f"output={args.output}")
    print(f"nodes_before={before_nodes}")
    print(f"nodes_after={len(model.graph.node)}")
    print(f"fused_gelu={fused}")
    print(f"removed_initializers={removed_initializers}")


if __name__ == "__main__":
    main()
