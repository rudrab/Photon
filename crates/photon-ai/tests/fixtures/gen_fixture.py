#!/usr/bin/env python3
"""
Generate a minimal identity ONNX model fixture for photon-ai tests.
Requires `onnx` package: `pip install onnx`.
"""

import sys
from pathlib import Path

try:
    import onnx
    from onnx import helper, TensorProto
except ImportError:
    print("Error: `onnx` package is required. Run `pip install onnx`.", file=sys.stderr)
    sys.exit(1)


def generate_identity_model(output_path: Path):
    # Define input [1, 3, 32, 32] float32
    input_info = helper.make_tensor_value_info('input', TensorProto.FLOAT, [1, 3, 32, 32])
    # Define output [1, 3, 32, 32] float32
    output_info = helper.make_tensor_value_info('output', TensorProto.FLOAT, [1, 3, 32, 32])

    # Identity node: input -> output
    node_def = helper.make_node(
        'Identity',
        inputs=['input'],
        outputs=['output'],
        name='identity_node',
    )

    # Graph
    graph_def = helper.make_graph(
        [node_def],
        'identity_graph',
        [input_info],
        [output_info],
    )

    # Model
    model_def = helper.make_model(
        graph_def,
        producer_name='photon-ai-tests',
        opset_imports=[helper.make_opsetid("", 13)],
    )

    onnx.checker.check_model(model_def)
    onnx.save(model_def, output_path)
    print(f"Generated identity ONNX model at: {output_path} ({output_path.stat().st_size} bytes)")


if __name__ == '__main__':
    fixture_dir = Path(__file__).resolve().parent
    out_file = fixture_dir / "identity.onnx"
    generate_identity_model(out_file)
