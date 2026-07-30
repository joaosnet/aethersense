"""
export_onnx.py — Standalone ONNX export script for AetherSense.

Loads a trained checkpoint, exports OccupancyHeatmapNet to ONNX opset 17,
validates against ONNXRuntime, and saves metadata + metrics.

Usage:
    uv run export_onnx.py --checkpoint ml/artifacts/runs/<run_id>/checkpoint_occupancy_heatmap_best.pt
    uv run export_onnx.py --checkpoint <path> --output-dir ml/artifacts/export/
"""

from __future__ import annotations

import argparse
import json
import sys
from datetime import datetime, timezone
from pathlib import Path

import numpy as np
import onnx
import onnxruntime as ort
import torch

from checkpointing import load_checkpoint
from config import Config
from logger import RunLogger
from models import OccupancyHeatmapNet


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Export AetherSense OccupancyHeatmapNet to ONNX",
        formatter_class=argparse.ArgumentDefaultsHelpFormatter,
    )
    parser.add_argument(
        "--checkpoint",
        type=Path,
        required=True,
        help="Path to a checkpoint .pt file.",
    )
    parser.add_argument(
        "--output-dir",
        type=Path,
        default=None,
        help="Output directory for .onnx and metadata files. "
             "Defaults to the checkpoint's parent directory.",
    )
    parser.add_argument(
        "--opset",
        type=int,
        default=17,
        help="ONNX opset version.",
    )
    return parser.parse_args()


def main() -> None:
    args = parse_args()

    checkpoint_path = args.checkpoint
    if not checkpoint_path.exists():
        print(f"[ERROR] Checkpoint not found: {checkpoint_path}", file=sys.stderr)
        sys.exit(1)

    output_dir = args.output_dir or checkpoint_path.parent
    output_dir.mkdir(parents=True, exist_ok=True)

    with RunLogger(output_dir) as log:
        log.info("ONNX export started", checkpoint=str(checkpoint_path))

        # --- Load config from checkpoint ---
        raw = torch.load(checkpoint_path, map_location="cpu", weights_only=False)
        ckpt_config = raw.get("config", {})
        heatmap_size = tuple(ckpt_config.get("heatmap_size", [20, 20]))
        target_subcarriers = int(ckpt_config.get("target_subcarriers", 64))
        # `to_dict()` serializa o campo `location_timesteps` (target_timesteps é
        # apenas uma property derivada); aceita ambos por retrocompatibilidade.
        target_timesteps = int(
            ckpt_config.get("location_timesteps",
                            ckpt_config.get("target_timesteps", 50))
        )
        max_count = int(ckpt_config.get("max_count", 3))
        location_anchors = ckpt_config.get("location_anchors", {
            "a": [0.18, 0.18], "b": [0.82, 0.18], "c": [0.50, 0.50],
            "d": [0.18, 0.82], "e": [0.82, 0.82],
        })

        # --- Build model and load weights ---
        device = torch.device("cuda" if torch.cuda.is_available() else "cpu")
        model = OccupancyHeatmapNet(
            heatmap_size=heatmap_size, max_count=max_count
        ).to(device)
        load_checkpoint(checkpoint_path, model, optimizer=None, device=device)
        model.eval()
        log.info(
            "Model loaded",
            epoch=raw.get("epoch", "?"),
            run_id=raw.get("run_id", "?"),
        )

        # --- Dummy forward pass ---
        dummy = torch.randn(1, 1, target_subcarriers, target_timesteps, device=device)
        with torch.no_grad():
            pt_logits, pt_heat = model(dummy)

        # --- ONNX export ---
        onnx_path = output_dir / "occupancy.onnx"
        torch.onnx.export(
            model,
            dummy,
            str(onnx_path),
            input_names=["csi"],
            output_names=["count_logits", "heatmap"],
            opset_version=args.opset,
            dynamo=False,
            do_constant_folding=True,
        )
        log.info(f"ONNX exported: {onnx_path}", opset=args.opset)

        # --- ONNX checker ---
        onnx_model = onnx.load(str(onnx_path))
        onnx.checker.check_model(onnx_model)
        log.info("ONNX checker passed")

        # --- ONNXRuntime validation ---
        session = ort.InferenceSession(
            str(onnx_path),
            providers=["CUDAExecutionProvider", "CPUExecutionProvider"],
        )
        ort_outputs = session.run(
            None, {"csi": dummy.detach().cpu().numpy().astype(np.float32)}
        )
        ort_logits, ort_heat = ort_outputs

        logit_diff = float(np.max(np.abs(pt_logits.detach().cpu().numpy() - ort_logits)))
        heat_diff  = float(np.max(np.abs(pt_heat.detach().cpu().numpy() - ort_heat)))
        log.info(
            "ORT validation",
            logit_max_abs_diff=logit_diff,
            heat_max_abs_diff=heat_diff,
        )

        # --- Metadata ---
        created_at = datetime.now(timezone.utc).isoformat()
        metadata = {
            "model_name":    "aethersense_occupancy_heatmap",
            "created_at":    created_at,
            "source_checkpoint": str(checkpoint_path),
            "source_epoch":  raw.get("epoch", -1),
            "source_run_id": raw.get("run_id", ""),
            "input_name":    "csi",
            "input_shape":   [1, 1, target_subcarriers, target_timesteps],
            "outputs":       ["count_logits", "heatmap"],
            "count_classes": [str(i) for i in range(max_count)] + [f"{max_count}+"],
            "max_count":     max_count,
            "heatmap_size":  list(heatmap_size),
            "location_anchors": location_anchors,
            "onnx_opset":    args.opset,
            "onnx_logit_max_abs_diff":  logit_diff,
            "onnx_heatmap_max_abs_diff": heat_diff,
            "ethical_constraint": (
                "No local human dataset collection; "
                "public datasets plus future empty-room calibration only."
            ),
        }
        metadata_path = output_dir / "occupancy_metadata.json"
        metadata_path.write_text(json.dumps(metadata, indent=2))
        log.info(f"Metadata saved: {metadata_path}")
        log.info("ONNX export complete.")

    print(f"\nArtifacts:\n  {onnx_path}\n  {metadata_path}")


if __name__ == "__main__":
    main()
