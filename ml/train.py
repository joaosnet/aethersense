"""
train.py — AetherSense ML: train ONE model end-to-end.

Trains OccupancyHeatmapNet on the WiMANS dataset, then exports to ONNX.
Running this script once does the full cycle: data → train → checkpoint → ONNX.

The model handles all detection tasks in a single forward pass:
  - count_logits : how many people (0, 1, …, max_count; last class = "max_count+")
  - heatmap      : where they are (Gaussian confidence map, cfg.heatmap_size)
Presence detection = count_logits.argmax() > 0.

Usage:
    # Full training
    uv run train.py

    # Smoke test (1 epoch, small dataset)
    uv run train.py --fast-dev

    # Custom epochs or data path
    uv run train.py --epochs 80 --data-dir /data/aethersense/datasets

    # Resume from checkpoint
    uv run train.py --resume ml/artifacts/runs/<run_id>/checkpoint_best.pt

    # Background execution (Linux / Thunder Compute)
    nohup uv run train.py --epochs 40 > nohup.out 2>&1 &
    tail -f nohup.out
"""

from __future__ import annotations

import json
import sys
import uuid
from datetime import datetime, timezone
from pathlib import Path
from typing import Dict, List, Optional

import numpy as np
import torch
import torch.nn.functional as F
from rich.console import Console
from rich.progress import (
    BarColumn,
    MofNCompleteColumn,
    Progress,
    SpinnerColumn,
    TaskProgressColumn,
    TextColumn,
    TimeElapsedColumn,
    TimeRemainingColumn,
)
from sklearn.metrics import (
    accuracy_score,
    confusion_matrix,
    precision_recall_fscore_support,
)
from torch.utils.data import DataLoader, random_split

from checkpointing import CheckpointManager, load_checkpoint
from config import Config
from datasets import WiMANSDataset, maybe_subset
from logger import RunLogger
from models import OccupancyHeatmapNet, parameter_count


# ---------------------------------------------------------------------------
# Rich progress bar
# ---------------------------------------------------------------------------

def _progress(console: Optional[Console] = None) -> Progress:
    """
    Creates a Rich progress bar.
    Automatically disables animations when stdout is not a TTY (nohup / pipes),
    but continues printing timestamped lines to the log.
    """
    return Progress(
        SpinnerColumn(),
        TextColumn("[progress.description]{task.description}"),
        BarColumn(bar_width=36),
        MofNCompleteColumn(),
        TaskProgressColumn(),
        TimeElapsedColumn(),
        TimeRemainingColumn(),
        console=console,
        refresh_per_second=2,
        disable=not (sys.stdout.isatty() or sys.stderr.isatty()),
    )


# ---------------------------------------------------------------------------
# Run ID
# ---------------------------------------------------------------------------

def _make_run_id() -> str:
    ts  = datetime.now(timezone.utc).strftime("%Y%m%d_%H%M%S")
    uid = uuid.uuid4().hex[:8]
    return f"{ts}_{uid}"


# ---------------------------------------------------------------------------
# Training
# ---------------------------------------------------------------------------

def train(
    cfg: Config,
    run_dir: Path,
    run_id: str,
    log: RunLogger,
    extra_datasets: Optional[list] = None,
) -> OccupancyHeatmapNet:
    """
    Full training loop for OccupancyHeatmapNet on WiMANS.

    *extra_datasets*: datasets auxiliares de ocupação (Wi-Pose, MM-Fi) com o
    mesmo contrato (csi, count, heatmap), concatenados ao WiMANS. Com dados
    auxiliares os pesos de classe são desativados (o cálculo é específico do
    annotation.csv do WiMANS).

    Returns the trained model (used for ONNX export).
    """
    # ------------------------------------------------------------------ data
    log.info("Loading WiMANS dataset …", path=str(cfg.wimans_dir))

    def _make_dataset(augment: bool):
        ds = WiMANSDataset(
            root=cfg.wimans_dir,
            target_subcarriers=cfg.target_subcarriers,
            target_timesteps=cfg.target_timesteps,
            heatmap_size=cfg.heatmap_size,
            location_anchors=cfg.location_anchors,
            max_count=cfg.max_count,
            augment=augment,
        )
        return maybe_subset(ds, cfg.fast_dev_max_samples, cfg.fast_dev)

    # Duas instâncias sobre os mesmos arquivos: o treino vê augmentation
    # (crop temporal, ruído, máscara de subportadoras); a validação vê os
    # dados limpos. O random_split com a MESMA semente e os mesmos tamanhos
    # produz a mesma partição de índices nas duas.
    dataset = _make_dataset(augment=True)
    clean_dataset = _make_dataset(augment=False)
    if extra_datasets:
        from torch.utils.data import ConcatDataset

        sizes = [len(d) for d in extra_datasets]
        dataset = ConcatDataset([dataset, *extra_datasets])
        clean_dataset = ConcatDataset([clean_dataset, *extra_datasets])
        log.info("Auxiliary occupancy datasets concatenated", extra_sizes=sizes)

    train_len = int(len(dataset) * cfg.train_frac)
    # Garante pelo menos 1 amostra de validação quando há ≥2 amostras —
    # caso contrário val_loss degenera (divisão vazia → checkpoint inválido).
    if len(dataset) > 1:
        train_len = max(1, min(train_len, len(dataset) - 1))
    val_len = len(dataset) - train_len
    train_ds, _ = random_split(
        dataset, [train_len, val_len],
        generator=torch.Generator().manual_seed(cfg.random_seed),
    )
    _, val_ds = random_split(
        clean_dataset, [train_len, val_len],
        generator=torch.Generator().manual_seed(cfg.random_seed),
    )

    # Multi-janela: repete cada gravação de treino K vezes por época; o crop
    # aleatório da augmentation gera uma janela diferente a cada repetição.
    # A validação fica intacta (1 janela determinística por gravação).
    k = max(1, cfg.wimans_windows_per_sample)
    if k > 1:
        from torch.utils.data import Subset

        expanded = [i for i in train_ds.indices for _ in range(k)]
        train_ds = Subset(dataset, expanded)
        log.info("Multi-janela ativo", windows_per_sample=k,
                 train_samples_per_epoch=len(expanded))
    log.info(
        f"Dataset ready — {len(dataset)} samples",
        train=train_len, val=val_len,
        batch_size=cfg.batch_size,
    )

    train_loader = DataLoader(
        train_ds, batch_size=cfg.batch_size, shuffle=True,
        num_workers=cfg.num_workers, pin_memory=(cfg.num_workers > 0),
    )
    val_loader = DataLoader(
        val_ds, batch_size=cfg.batch_size, shuffle=False,
        num_workers=cfg.num_workers,
    )

    # ----------------------------------------------------------------- model
    device = torch.device("cuda" if torch.cuda.is_available() else "cpu")
    model  = OccupancyHeatmapNet(
        heatmap_size=cfg.heatmap_size, max_count=cfg.max_count
    ).to(device)
    optimizer = torch.optim.AdamW(model.parameters(), lr=cfg.learning_rate)
    log.info(
        f"OccupancyHeatmapNet — {parameter_count(model):,} params",
        device=str(device), max_count=cfg.max_count,
    )

    # ── Pesos de classe (compensam o desbalanceamento da contagem) ──────────
    num_classes = cfg.max_count + 1
    class_weights: Optional[torch.Tensor] = None
    if cfg.use_class_weights and isinstance(dataset, WiMANSDataset):
        all_counts = np.asarray(dataset.count_labels())
        train_counts = all_counts[np.asarray(train_ds.indices)]
        freq = np.bincount(train_counts, minlength=num_classes).astype(np.float64)
        inv = 1.0 / np.maximum(freq, 1.0)
        weights = inv / inv.sum() * num_classes        # média ≈ 1.0
        class_weights = torch.tensor(weights, dtype=torch.float32, device=device)
        log.info(
            "Class weights computed",
            class_freq=[int(f) for f in freq],
            weights=[round(float(w), 3) for w in weights],
        )

    # ── Loss de contagem: cross-entropy + termo ordinal ─────────────────────
    # Contagem tem ordem: errar "2" prevendo "3" é melhor que prevendo "5".
    # O termo ordinal aproxima a contagem esperada (softmax · classes) do alvo.
    class_values = torch.arange(num_classes, dtype=torch.float32, device=device)

    def count_loss(logits: torch.Tensor, count: torch.Tensor) -> torch.Tensor:
        ce = F.cross_entropy(logits, count, weight=class_weights)
        if cfg.count_ordinal_weight <= 0:
            return ce
        expected = (F.softmax(logits, dim=1) * class_values).sum(dim=1)
        return ce + cfg.count_ordinal_weight * F.l1_loss(expected, count.float())

    start_epoch = 1

    # --------------------------------------------------------------- resume
    if cfg.resume and cfg.resume.exists():
        payload = load_checkpoint(cfg.resume, model, optimizer, device)
        start_epoch = payload["epoch"] + 1
        log.info(
            "Resumed from checkpoint",
            path=str(cfg.resume),
            from_epoch=payload["epoch"],
            best_val_loss=payload.get("best_val_loss", "?"),
        )

    # --------------------------------------------------- checkpoint manager
    ckpt_mgr = CheckpointManager(
        run_dir=run_dir,
        model_name="occupancy_heatmap",
        config_dict=cfg.to_dict(),
        run_id=run_id,
        checkpoint_every=cfg.checkpoint_every_n_epochs,
        lower_is_better=True,
        metric_key="val_loss",
    )

    console = Console()

    # Inicializados fora do loop: ao retomar um checkpoint sem épocas
    # restantes (start_epoch > epochs) o loop não executa e o resumo final
    # ainda referencia estas variáveis.
    y_true: List[int] = []
    y_pred: List[int] = []

    # -------------------------------------------------------- training loop
    for epoch in range(start_epoch, cfg.epochs + 1):

        # ── Train ──────────────────────────────────────────────────────────
        model.train()
        train_losses: List[float] = []

        with _progress(console) as prog:
            task = prog.add_task(
                f"[magenta]Epoch {epoch:3d}/{cfg.epochs}  train",
                total=len(train_loader),
            )
            for x, count, heat in train_loader:
                x, count, heat = x.to(device), count.to(device), heat.to(device)

                logits, pred_heat = model(x)
                loss_count = count_loss(logits, count)
                loss_heat  = F.mse_loss(pred_heat, heat)
                loss = (cfg.heatmap_loss_weight * loss_heat
                        + cfg.count_loss_weight  * loss_count)

                optimizer.zero_grad()
                loss.backward()
                optimizer.step()

                train_losses.append(float(loss.detach().cpu()))
                prog.advance(task)

        # ── Validate ────────────────────────────────────────────────────────
        model.eval()
        y_true, y_pred, val_losses, heat_mse_list = [], [], [], []

        with torch.no_grad():
            with _progress(console) as prog:
                task = prog.add_task(
                    f"[blue  ]Epoch {epoch:3d}/{cfg.epochs}  val  ",
                    total=len(val_loader),
                )
                for x, count, heat in val_loader:
                    # Mover todos os tensores para o device (consistência GPU/CPU).
                    x, count, heat = x.to(device), count.to(device), heat.to(device)
                    logits, pred_heat = model(x)

                    # Mesma loss combinada do treino — torna val_loss comparável
                    # e faz o "best checkpoint" considerar contagem + heatmap.
                    loss_count = count_loss(logits, count)
                    loss_heat  = F.mse_loss(pred_heat, heat)
                    loss = (cfg.heatmap_loss_weight * loss_heat
                            + cfg.count_loss_weight  * loss_count)

                    val_losses.append(float(loss.detach().cpu()))
                    heat_mse_list.append(float(loss_heat.detach().cpu()))
                    y_true.extend(count.cpu().tolist())
                    y_pred.extend(logits.argmax(1).cpu().tolist())
                    prog.advance(task)

        # ── Metrics ─────────────────────────────────────────────────────────
        train_loss   = float(np.mean(train_losses))  if train_losses  else 0.0
        val_loss     = float(np.mean(val_losses))    if val_losses    else float("inf")
        val_heat_mse = float(np.mean(heat_mse_list)) if heat_mse_list else float("inf")
        val_acc      = float(accuracy_score(y_true, y_pred)) if y_true else 0.0

        metrics = {
            "train_loss":       train_loss,
            "val_loss":         val_loss,          # combined loss — drives checkpoint
            "val_heat_mse":     val_heat_mse,      # heatmap-only MSE (diagnostic)
            "val_count_acc":    val_acc,
        }
        log.metric(epoch=epoch, **metrics)

        ckpt_mgr.step(
            epoch=epoch, model=model, optimizer=optimizer,
            metrics=metrics, log=log,
        )

    # ─── Final summary ──────────────────────────────────────────────────────
    summary = ckpt_mgr.summary()
    labels = list(range(num_classes))
    conf_mat: list = []
    per_class: dict = {}
    if y_true:
        conf_mat = confusion_matrix(y_true, y_pred, labels=labels).tolist()
        precision, recall, f1, support = precision_recall_fscore_support(
            y_true, y_pred, labels=labels, zero_division=0
        )
        # Métricas por classe de contagem — essenciais sob desbalanceamento,
        # pois a acurácia global esconde classes raras (ex.: 0 ou 5 pessoas).
        per_class = {
            str(c): {
                "precision": round(float(precision[i]), 4),
                "recall":    round(float(recall[i]), 4),
                "f1":        round(float(f1[i]), 4),
                "support":   int(support[i]),
            }
            for i, c in enumerate(labels)
        }
        log.info("Per-class count metrics", **{
            f"class_{c}_f1": per_class[str(c)]["f1"] for c in labels
        })

    log.info(
        "Training complete",
        best_val_loss=summary["best_val"],
        best_epoch=summary["best_epoch"],
        best_checkpoint=summary["best_checkpoint"],
    )

    # Recarrega o MELHOR checkpoint antes de retornar: o export ONNX deve usar
    # o modelo da melhor val_loss, não o da última época (que pode ter
    # overfittado — ex.: val 0.79 na época 21 vs 1.28 na 40).
    if ckpt_mgr.best_checkpoint_path is not None:
        load_checkpoint(ckpt_mgr.best_checkpoint_path, model, device=device)
        log.info("Best checkpoint reloaded for export",
                 epoch=summary["best_epoch"])

    return model, summary, conf_mat, per_class


# ---------------------------------------------------------------------------
# ONNX export
# ---------------------------------------------------------------------------

def export_onnx(
    model: OccupancyHeatmapNet,
    cfg: Config,
    run_dir: Path,
    log: RunLogger,
) -> Path:
    """
    Export the trained model to ONNX opset 17 and validate with ONNXRuntime.
    Returns the path to the exported .onnx file.
    """
    import onnx
    import onnxruntime as ort

    device = next(model.parameters()).device
    model.eval()

    dummy = torch.randn(
        1, 1, cfg.target_subcarriers, cfg.target_timesteps, device=device
    )

    onnx_path = run_dir / "occupancy.onnx"

    torch.onnx.export(
        model,
        dummy,
        str(onnx_path),
        input_names=["csi"],
        output_names=["count_logits", "heatmap"],
        opset_version=17,
        dynamo=False,
        do_constant_folding=True,
    )
    log.info("ONNX exported", path=str(onnx_path))

    # Validate model graph
    onnx.checker.check_model(onnx.load(str(onnx_path)))
    log.info("ONNX checker passed")

    # Cross-validate PyTorch vs ONNXRuntime
    session = ort.InferenceSession(
        str(onnx_path),
        providers=["CUDAExecutionProvider", "CPUExecutionProvider"],
    )
    with torch.no_grad():
        pt_logits, pt_heat = model(dummy)
    ort_logits, ort_heat = session.run(
        None, {"csi": dummy.detach().cpu().numpy().astype(np.float32)}
    )
    logit_diff = float(np.max(np.abs(pt_logits.detach().cpu().numpy() - ort_logits)))
    heat_diff  = float(np.max(np.abs(pt_heat.detach().cpu().numpy() - ort_heat)))
    log.info(
        "ORT validation",
        logit_max_abs_diff=logit_diff,
        heatmap_max_abs_diff=heat_diff,
    )

    return onnx_path, logit_diff, heat_diff


# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------

def main() -> None:
    cfg = Config.from_args()

    torch.manual_seed(cfg.random_seed)
    np.random.seed(cfg.random_seed)

    run_id  = _make_run_id()
    run_dir = cfg.output_dir / "runs" / run_id
    run_dir.mkdir(parents=True, exist_ok=True)

    # Persist config for reproducibility
    cfg.save(run_dir / "config.json")

    with RunLogger(run_dir) as log:
        log.info(f"AetherSense ML — run started")
        log.info(f"Run ID    : {run_id}")
        log.info(f"Output    : {run_dir}")
        log.info(f"Epochs    : {cfg.epochs}")
        log.info(f"Fast dev  : {cfg.fast_dev}")
        log.info(f"Resume    : {cfg.resume or '—'}")

        # ── Check dataset availability ──────────────────────────────────────
        if not next(cfg.wimans_dir.glob("**/annotation.csv"), None):
            log.error(
                "WiMANS annotation.csv not found. "
                "Run `uv run download_datasets.py` (or `--smoke`) and retry.",
                path=str(cfg.wimans_dir),
            )
            raise SystemExit(1)

        # ── Train ───────────────────────────────────────────────────────────
        model, train_summary, conf_mat, per_class = train(cfg, run_dir, run_id, log)

        # ── Export ONNX ─────────────────────────────────────────────────────
        log.info("Starting ONNX export …")
        onnx_path, logit_diff, heat_diff = export_onnx(model, cfg, run_dir, log)

        # ── Metadata ────────────────────────────────────────────────────────
        created_at = datetime.now(timezone.utc).isoformat()
        metadata = {
            "model_name":    "aethersense_occupancy_heatmap",
            "created_at":    created_at,
            "run_id":        run_id,
            "input_name":    "csi",
            "input_shape":   [1, 1, cfg.target_subcarriers, cfg.target_timesteps],
            "outputs":       ["count_logits", "heatmap"],
            "count_classes": [str(i) for i in range(cfg.max_count)] + [f"{cfg.max_count}+"],
            "max_count":     cfg.max_count,
            "heatmap_size":  list(cfg.heatmap_size),
            "location_anchors": cfg.location_anchors,
            "onnx_opset":   17,
            "onnx_logit_max_abs_diff":   logit_diff,
            "onnx_heatmap_max_abs_diff": heat_diff,
            "ethical_constraint": (
                "No local human dataset collection; "
                "public datasets plus future empty-room calibration only."
            ),
        }
        (run_dir / "occupancy_metadata.json").write_text(
            json.dumps(metadata, indent=2)
        )

        # ── Final metrics summary ───────────────────────────────────────────
        metrics_summary = {
            "run_id":        run_id,
            "created_at":    created_at,
            "fast_dev":      cfg.fast_dev,
            "epochs_total":  cfg.epochs,
            "best_val_loss": train_summary["best_val"],
            "best_epoch":    train_summary["best_epoch"],
            "best_checkpoint": train_summary["best_checkpoint"],
            "confusion_matrix": conf_mat,
            "per_class_count_metrics": per_class,
            "onnx_logit_max_abs_diff":   logit_diff,
            "onnx_heatmap_max_abs_diff": heat_diff,
            "input_shape":   [1, 1, cfg.target_subcarriers, cfg.target_timesteps],
        }
        metrics_path = run_dir / "metrics_summary.json"
        metrics_path.write_text(json.dumps(metrics_summary, indent=2))

        log.info("─" * 60)
        log.info(f"Run complete. Artifacts saved to: {run_dir}")
        log.info(f"  Model   : {run_dir / 'checkpoint_occupancy_heatmap_best.pt'}")
        log.info(f"  ONNX    : {onnx_path}")
        log.info(f"  Metrics : {metrics_path}")
        log.info(f"  Log     : {run_dir / 'log.txt'}")


if __name__ == "__main__":
    main()
