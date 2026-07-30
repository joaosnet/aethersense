"""
trainers.py — train loops for PresenceCNN and VitalsNet.

OccupancyHeatmapNet (the main model) keeps its loop in train.py; this module
adds the two satellite models from docs/ml-pipeline-status.md:

  - train_presence : PresenceCNN on Zenodo 8021099 (binary presence)
  - train_vitals   : VitalsNet on vitals datasets (breathing + heart rate)

Both follow the same conventions as train.py: CheckpointManager for best/
periodic checkpoints, RunLogger METRIC events per epoch, Rich progress bars,
and ONNX export validated against ONNXRuntime at the end.
"""

from __future__ import annotations

from pathlib import Path
from typing import List, Optional, Sequence, Tuple

import numpy as np
import torch
import torch.nn.functional as F
from rich.console import Console
from sklearn.metrics import (
    accuracy_score,
    confusion_matrix,
    precision_recall_fscore_support,
)
from torch.utils.data import ConcatDataset, DataLoader, Dataset, random_split

from checkpointing import CheckpointManager
from config import Config
from datasets import ZenodoPresenceDataset, maybe_subset
from logger import RunLogger
from models import PresenceCNN, VitalsNet, parameter_count
from train import _progress


# ---------------------------------------------------------------------------
# Generic ONNX export + ORT cross-validation
# ---------------------------------------------------------------------------

def export_model_onnx(
    model: torch.nn.Module,
    dummy: torch.Tensor,
    onnx_path: Path,
    output_names: List[str],
    log: RunLogger,
) -> Tuple[Path, float]:
    """Export *model* to ONNX opset 17 and cross-validate with ONNXRuntime.

    Returns (path, max_abs_diff PyTorch×ORT sobre todas as saídas).
    """
    import onnx
    import onnxruntime as ort

    model.eval()
    torch.onnx.export(
        model,
        dummy,
        str(onnx_path),
        input_names=["csi"],
        output_names=output_names,
        opset_version=17,
        dynamo=False,
        do_constant_folding=True,
    )
    onnx.checker.check_model(onnx.load(str(onnx_path)))

    session = ort.InferenceSession(
        str(onnx_path),
        providers=["CUDAExecutionProvider", "CPUExecutionProvider"],
    )
    with torch.no_grad():
        pt_out = model(dummy)
    pt_outs = pt_out if isinstance(pt_out, tuple) else (pt_out,)
    ort_outs = session.run(None, {"csi": dummy.detach().cpu().numpy().astype(np.float32)})
    max_diff = max(
        float(np.max(np.abs(p.detach().cpu().numpy() - o)))
        for p, o in zip(pt_outs, ort_outs)
    )
    log.info("ONNX exported + ORT validated",
             path=str(onnx_path), max_abs_diff=max_diff)
    return onnx_path, max_diff


def _split(dataset: Dataset, train_frac: float, seed: int):
    train_len = int(len(dataset) * train_frac)
    if len(dataset) > 1:
        train_len = max(1, min(train_len, len(dataset) - 1))
    return random_split(
        dataset, [train_len, len(dataset) - train_len],
        generator=torch.Generator().manual_seed(seed),
    )


# ---------------------------------------------------------------------------
# PresenceCNN — Zenodo 8021099
# ---------------------------------------------------------------------------

def train_presence(
    cfg: Config, run_dir: Path, run_id: str, log: RunLogger
) -> Tuple[PresenceCNN, dict]:
    """Treina PresenceCNN no Zenodo e exporta presence.onnx.

    Returns (model, metrics dict para o benchmark/relatório).
    """
    log.info("Loading Zenodo dataset …", path=str(cfg.zenodo_dir))
    dataset = ZenodoPresenceDataset(cfg.zenodo_dir, split="train")
    dataset = maybe_subset(dataset, cfg.fast_dev_max_samples, cfg.fast_dev)
    train_ds, val_ds = _split(dataset, cfg.train_frac, cfg.random_seed)
    log.info(f"Zenodo ready — {len(dataset)} samples",
             train=len(train_ds), val=len(val_ds))

    device = torch.device("cuda" if torch.cuda.is_available() else "cpu")
    model = PresenceCNN().to(device)
    optimizer = torch.optim.AdamW(model.parameters(), lr=cfg.learning_rate)
    log.info(f"PresenceCNN — {parameter_count(model):,} params", device=str(device))

    train_loader = DataLoader(train_ds, batch_size=cfg.batch_size, shuffle=True,
                              num_workers=cfg.num_workers)
    val_loader = DataLoader(val_ds, batch_size=cfg.batch_size, shuffle=False,
                            num_workers=cfg.num_workers)

    ckpt_mgr = CheckpointManager(
        run_dir=run_dir, model_name="presence_cnn", config_dict=cfg.to_dict(),
        run_id=run_id, checkpoint_every=cfg.checkpoint_every_n_epochs,
        lower_is_better=True, metric_key="val_loss",
    )
    console = Console()

    y_true: List[int] = []
    y_pred: List[int] = []
    for epoch in range(1, cfg.presence_epochs + 1):
        model.train()
        train_losses: List[float] = []
        with _progress(console) as prog:
            task = prog.add_task(
                f"[magenta]Presence {epoch:2d}/{cfg.presence_epochs} train",
                total=len(train_loader))
            for x, y in train_loader:
                x, y = x.to(device), y.to(device)
                loss = F.cross_entropy(model(x), y)
                optimizer.zero_grad()
                loss.backward()
                optimizer.step()
                train_losses.append(float(loss.detach().cpu()))
                prog.advance(task)

        model.eval()
        y_true, y_pred, val_losses = [], [], []
        with torch.no_grad():
            for x, y in val_loader:
                x, y = x.to(device), y.to(device)
                logits = model(x)
                val_losses.append(float(F.cross_entropy(logits, y).cpu()))
                y_true.extend(y.cpu().tolist())
                y_pred.extend(logits.argmax(1).cpu().tolist())

        metrics = {
            "model": "presence_cnn",
            "train_loss": float(np.mean(train_losses)) if train_losses else 0.0,
            "val_loss": float(np.mean(val_losses)) if val_losses else float("inf"),
            "val_acc": float(accuracy_score(y_true, y_pred)) if y_true else 0.0,
        }
        log.metric(epoch=epoch, **metrics)
        ckpt_mgr.step(epoch=epoch, model=model, optimizer=optimizer,
                      metrics=metrics, log=log)

    precision, recall, f1, _ = precision_recall_fscore_support(
        y_true, y_pred, labels=[0, 1], zero_division=0)
    summary = {
        "best_val_loss": ckpt_mgr.summary()["best_val"],
        "val_acc": float(accuracy_score(y_true, y_pred)) if y_true else 0.0,
        "precision": [round(float(p), 4) for p in precision],
        "recall": [round(float(r), 4) for r in recall],
        "f1": [round(float(f), 4) for f in f1],
        "confusion_matrix": confusion_matrix(y_true, y_pred, labels=[0, 1]).tolist(),
    }
    log.info("Presence training complete", **{
        k: v for k, v in summary.items() if k != "confusion_matrix"})

    # Export usa o melhor checkpoint, não a última época
    if ckpt_mgr.best_checkpoint_path is not None:
        from checkpointing import load_checkpoint
        load_checkpoint(ckpt_mgr.best_checkpoint_path, model, device=device)

    dummy = torch.randn(1, 1, 64, 64, device=device)
    _, diff = export_model_onnx(
        model, dummy, run_dir / "presence.onnx", ["presence_logits"], log)
    summary["onnx_max_abs_diff"] = diff
    return model, summary


# ---------------------------------------------------------------------------
# VitalsNet — CSI-MiningTool + NIST BreatheSmart
# ---------------------------------------------------------------------------

def _masked_l1(pred: torch.Tensor, target: torch.Tensor) -> torch.Tensor:
    """L1 apenas nos canais com ground truth (NaN = sem label, ex.: NIST
    não anota batimento). Sem nenhum alvo finito, retorna 0 com grad."""
    mask = torch.isfinite(target)
    if not mask.any():
        return pred.sum() * 0.0
    return F.l1_loss(pred[mask], target[mask])


def _masked_mae_per_min(preds: np.ndarray, trues: np.ndarray, col: int) -> Optional[float]:
    """MAE em unidades por-minuto (RPM/BPM), ignorando alvos NaN."""
    if not len(preds):
        return None
    finite = np.isfinite(trues[:, col])
    if not finite.any():
        return None
    return float(np.mean(np.abs(preds[finite, col] - trues[finite, col])) * 60)


def train_vitals(
    cfg: Config,
    run_dir: Path,
    run_id: str,
    log: RunLogger,
    datasets: Sequence[Dataset],
) -> Tuple[Optional[VitalsNet], dict]:
    """Treina VitalsNet na concatenação dos datasets de vitais disponíveis.

    Cada dataset deve produzir (csi (1, 64, T), target (2,) em Hz).
    Loss L1 em Hz; métricas reportadas em RPM (respiração) e BPM (coração).
    """
    if not datasets:
        log.info("VitalsNet skipped — nenhum dataset de vitais disponível")
        return None, {"skipped": True}

    dataset: Dataset = (
        datasets[0] if len(datasets) == 1 else ConcatDataset(list(datasets))
    )
    dataset = maybe_subset(dataset, cfg.fast_dev_max_samples, cfg.fast_dev)
    train_ds, val_ds = _split(dataset, cfg.train_frac, cfg.random_seed)
    log.info(f"Vitals ready — {len(dataset)} windows",
             sources=len(datasets), train=len(train_ds), val=len(val_ds))

    device = torch.device("cuda" if torch.cuda.is_available() else "cpu")
    model = VitalsNet(subcarriers=cfg.target_subcarriers).to(device)
    optimizer = torch.optim.AdamW(model.parameters(), lr=cfg.learning_rate)
    log.info(f"VitalsNet — {parameter_count(model):,} params", device=str(device))

    train_loader = DataLoader(train_ds, batch_size=cfg.batch_size, shuffle=True,
                              num_workers=cfg.num_workers)
    val_loader = DataLoader(val_ds, batch_size=cfg.batch_size, shuffle=False,
                            num_workers=cfg.num_workers)

    ckpt_mgr = CheckpointManager(
        run_dir=run_dir, model_name="vitals_net", config_dict=cfg.to_dict(),
        run_id=run_id, checkpoint_every=cfg.checkpoint_every_n_epochs,
        lower_is_better=True, metric_key="val_loss",
    )
    console = Console()

    preds = np.zeros((0, 2), dtype=np.float32)
    trues = np.zeros((0, 2), dtype=np.float32)
    for epoch in range(1, cfg.vitals_epochs + 1):
        model.train()
        train_losses: List[float] = []
        with _progress(console) as prog:
            task = prog.add_task(
                f"[magenta]Vitals   {epoch:2d}/{cfg.vitals_epochs} train",
                total=len(train_loader))
            for x, target in train_loader:
                x, target = x.to(device), target.to(device)
                loss = _masked_l1(model(x), target)
                optimizer.zero_grad()
                loss.backward()
                optimizer.step()
                train_losses.append(float(loss.detach().cpu()))
                prog.advance(task)

        model.eval()
        val_losses, p_list, t_list = [], [], []
        with torch.no_grad():
            for x, target in val_loader:
                x, target = x.to(device), target.to(device)
                out = model(x)
                val_losses.append(float(_masked_l1(out, target).cpu()))
                p_list.append(out.cpu().numpy())
                t_list.append(target.cpu().numpy())

        preds = np.concatenate(p_list) if p_list else np.zeros((0, 2), np.float32)
        trues = np.concatenate(t_list) if t_list else np.zeros((0, 2), np.float32)
        # Hz → por minuto: respiração em RPM, coração em BPM (NaN ignorado)
        mae_rpm = _masked_mae_per_min(preds, trues, 0)
        mae_bpm = _masked_mae_per_min(preds, trues, 1)

        metrics = {
            "model": "vitals_net",
            "train_loss": float(np.mean(train_losses)) if train_losses else 0.0,
            "val_loss": float(np.mean(val_losses)) if val_losses else float("inf"),
            "val_mae_breathing_rpm": mae_rpm,
            "val_mae_heart_bpm": mae_bpm,
        }
        log.metric(epoch=epoch, **metrics)
        ckpt_mgr.step(epoch=epoch, model=model, optimizer=optimizer,
                      metrics=metrics, log=log)

    def _pearson(col: int) -> Optional[float]:
        if not len(preds):
            return None
        finite = np.isfinite(trues[:, col])
        a, b = preds[finite, col], trues[finite, col]
        if len(a) < 2 or float(np.std(a)) == 0.0 or float(np.std(b)) == 0.0:
            return None
        return float(np.corrcoef(a, b)[0, 1])

    summary = {
        "best_val_loss": ckpt_mgr.summary()["best_val"],
        "mae_breathing_rpm": _masked_mae_per_min(preds, trues, 0),
        "mae_heart_bpm": _masked_mae_per_min(preds, trues, 1),
        "pearson_breathing": _pearson(0),
        "pearson_heart": _pearson(1),
    }
    log.info("Vitals training complete", **summary)

    # Export usa o melhor checkpoint, não a última época
    if ckpt_mgr.best_checkpoint_path is not None:
        from checkpointing import load_checkpoint
        load_checkpoint(ckpt_mgr.best_checkpoint_path, model, device=device)

    dummy = torch.randn(1, 1, cfg.target_subcarriers, cfg.breathing_timesteps,
                        device=device)
    _, diff = export_model_onnx(
        model, dummy, run_dir / "vitals.onnx", ["vitals_hz"], log)
    summary["onnx_max_abs_diff"] = diff
    return model, summary
