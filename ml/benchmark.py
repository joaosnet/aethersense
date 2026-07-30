"""
benchmark.py — offline benchmark framework for the AetherSense models.

Avalia os modelos ONNX EXPORTADOS (não os pesos PyTorch) — é o artefato que o
servidor Rust consome, então as métricas e a latência medidas aqui refletem o
que chega em produção. Métricas conforme docs/ml-pipeline-status.md:

  Presença     : Accuracy, Precision, Recall, F1, Confusion Matrix
  Contagem     : MAE, Accuracy por classe (0-5+), Confusion Matrix
  Localização  : Mean Distance Error (m), CDF de erro (0.5 m, 1 m, 2 m)
  Vitais       : MAE (RPM/BPM), correlação de Pearson, dados p/ Bland-Altman
  Latência     : tempo médio de inferência por frame (ms), batch=1

Importante: ocupação e vitais são avaliados SOMENTE no split de validação
(mesma semente/fração do treino via trainers._split) — avaliar o dataset
inteiro contaminaria as métricas com amostras vistas no treino. A presença
usa o split de teste do próprio Zenodo (testLabels.csv), que é independente.

O modo "ao vivo" (ESP32) é Fase 3 — run_live_benchmark é um stub explícito.

As distâncias de localização são calculadas em coordenadas normalizadas [0,1]
e convertidas para metros assumindo sala quadrada de ROOM_SIZE_M de lado
(os anchors a–e do WiMANS não publicam dimensões exatas; o valor é uma
aproximação documentada, ajustável por parâmetro).
"""

from __future__ import annotations

import time
from pathlib import Path
from typing import Dict, List, Optional, Sequence

import numpy as np
import torch
from sklearn.metrics import (
    accuracy_score,
    confusion_matrix,
    precision_recall_fscore_support,
)
from torch.utils.data import Dataset

from config import Config
from datasets import WiMANSDataset, ZenodoPresenceDataset, maybe_subset
from logger import RunLogger
from preprocessing import heatmap_to_persons

ROOM_SIZE_M = 6.0          # lado assumido da sala (m) p/ converter erro 0-1 → m
CDF_THRESHOLDS_M = (0.5, 1.0, 2.0)
_LATENCY_RUNS = 100
_LATENCY_WARMUP = 10


def _session(onnx_path: Path):
    import onnxruntime as ort
    return ort.InferenceSession(
        str(onnx_path),
        providers=["CUDAExecutionProvider", "CPUExecutionProvider"],
    )


def _measure_latency(session, sample: np.ndarray) -> float:
    """Latência média de inferência (ms) com batch=1."""
    feed = {"csi": sample}
    for _ in range(_LATENCY_WARMUP):
        session.run(None, feed)
    start = time.perf_counter()
    for _ in range(_LATENCY_RUNS):
        session.run(None, feed)
    return (time.perf_counter() - start) / _LATENCY_RUNS * 1000.0


# ---------------------------------------------------------------------------
# Presença — presence.onnx × Zenodo (split de teste)
# ---------------------------------------------------------------------------

def benchmark_presence(cfg: Config, onnx_path: Path, log: RunLogger) -> dict:
    session = _session(onnx_path)
    dataset = ZenodoPresenceDataset(cfg.zenodo_dir, split="test")
    dataset = maybe_subset(dataset, cfg.fast_dev_max_samples, cfg.fast_dev)

    y_true: List[int] = []
    y_pred: List[int] = []
    latency = None
    for i in range(len(dataset)):
        x, y = dataset[i]
        feed = x.unsqueeze(0).numpy().astype(np.float32)
        if latency is None:
            latency = _measure_latency(session, feed)
        (logits,) = session.run(None, {"csi": feed})
        y_true.append(int(y))
        y_pred.append(int(np.argmax(logits)))

    precision, recall, f1, _ = precision_recall_fscore_support(
        y_true, y_pred, labels=[0, 1], zero_division=0)
    result = {
        "samples": len(y_true),
        "accuracy": float(accuracy_score(y_true, y_pred)),
        "precision": [round(float(p), 4) for p in precision],
        "recall": [round(float(r), 4) for r in recall],
        "f1": [round(float(f), 4) for f in f1],
        "confusion_matrix": confusion_matrix(y_true, y_pred, labels=[0, 1]).tolist(),
        "latency_ms": latency,
    }
    log.info("Benchmark presença", samples=result["samples"],
             accuracy=result["accuracy"], latency_ms=latency)
    return result


# ---------------------------------------------------------------------------
# Ocupação — occupancy.onnx × WiMANS (contagem + localização)
# ---------------------------------------------------------------------------

def _resolve_base(dataset, idx: int):
    """Atravessa Subsets aninhados até o dataset base e o índice real."""
    from torch.utils.data import Subset

    while isinstance(dataset, Subset):
        idx = dataset.indices[idx]
        dataset = dataset.dataset
    return dataset, idx


def _softmax(logits: np.ndarray) -> np.ndarray:
    e = np.exp(logits - np.max(logits))
    return e / e.sum()


def _match_distances(
    gt_points: List[dict], pred_points: List[dict]
) -> List[float]:
    """Pareamento guloso GT↔pred por menor distância (coords normalizadas)."""
    distances: List[float] = []
    remaining = list(pred_points)
    for gt in gt_points:
        if not remaining:
            break
        dists = [
            float(np.hypot(gt["x"] - p["x"], gt["y"] - p["y"]))
            for p in remaining
        ]
        best = int(np.argmin(dists))
        distances.append(dists[best])
        remaining.pop(best)
    return distances


def benchmark_occupancy(
    cfg: Config, onnx_path: Path, log: RunLogger,
    room_size_m: float = ROOM_SIZE_M,
) -> dict:
    from trainers import _split

    session = _session(onnx_path)
    dataset = WiMANSDataset(
        root=cfg.wimans_dir,
        target_subcarriers=cfg.target_subcarriers,
        target_timesteps=cfg.target_timesteps,
        heatmap_size=cfg.heatmap_size,
        location_anchors=cfg.location_anchors,
        max_count=cfg.max_count,
    )
    dataset = maybe_subset(dataset, cfg.fast_dev_max_samples, cfg.fast_dev)
    # Apenas o split de validação — mesma semente/fração do treino
    _, dataset = _split(dataset, cfg.train_frac, cfg.random_seed)

    y_true: List[int] = []
    y_pred: List[int] = []
    distances_m: List[float] = []
    gt_misses = 0
    gt_total = 0
    latency = None

    tta_used = 1
    for i in range(len(dataset)):
        x, count, gt_heat = dataset[i]

        # Test-time augmentation: vota (média do softmax) sobre janelas
        # temporais deslocadas da mesma gravação. Cada janela tem o mesmo
        # shape do feed live — o contrato (1, 64, 50) não muda.
        base, base_idx = _resolve_base(dataset, i)
        if isinstance(base, WiMANSDataset):
            windows = base.tta_windows(base_idx)
        else:
            windows = [x]
        tta_used = len(windows)

        feeds = [w.unsqueeze(0).numpy().astype(np.float32) for w in windows]
        if latency is None:
            latency = _measure_latency(session, feeds[0])
        probs, heats = [], []
        for feed in feeds:
            logits, pred_heat = session.run(None, {"csi": feed})
            probs.append(_softmax(logits[0]))
            heats.append(pred_heat[0, 0])
        mean_probs = np.mean(probs, axis=0)
        mean_heat = np.mean(heats, axis=0)

        y_true.append(int(count))
        y_pred.append(int(np.argmax(mean_probs)))

        # Localização: decodifica picos do heatmap GT e do predito com o mesmo
        # decoder usado em produção; erro = pareamento guloso em metros.
        gt_pts = heatmap_to_persons(gt_heat.squeeze(0).numpy(), max_people=cfg.max_count)
        pred_pts = heatmap_to_persons(mean_heat, max_people=cfg.max_count)
        matched = _match_distances(gt_pts, pred_pts)
        distances_m.extend(d * room_size_m for d in matched)
        gt_total += len(gt_pts)
        gt_misses += len(gt_pts) - len(matched)

    labels = list(range(cfg.max_count + 1))
    _, recall_cls, _, support = precision_recall_fscore_support(
        y_true, y_pred, labels=labels, zero_division=0)
    dist_arr = np.asarray(distances_m, dtype=np.float64)
    cdf = {
        f"{t}m": (float(np.mean(dist_arr <= t)) if len(dist_arr) else None)
        for t in CDF_THRESHOLDS_M
    }
    result = {
        "samples": len(y_true),
        "count_mae": float(np.mean(np.abs(np.array(y_true) - np.array(y_pred)))) if y_true else None,
        "count_accuracy": float(accuracy_score(y_true, y_pred)) if y_true else None,
        "count_acc_per_class": {
            str(c): {"recall": round(float(recall_cls[i]), 4),
                     "support": int(support[i])}
            for i, c in enumerate(labels)
        },
        "confusion_matrix": confusion_matrix(y_true, y_pred, labels=labels).tolist(),
        "mean_distance_error_m": float(dist_arr.mean()) if len(dist_arr) else None,
        "location_cdf": cdf,
        "location_distances_m": [round(float(d), 4) for d in dist_arr],
        "location_miss_rate": (gt_misses / gt_total) if gt_total else None,
        "room_size_m_assumed": room_size_m,
        "tta_windows": tta_used,
        "latency_ms": latency,
    }
    log.info("Benchmark ocupação", samples=result["samples"],
             count_mae=result["count_mae"],
             mean_distance_error_m=result["mean_distance_error_m"],
             latency_ms=latency)
    return result


# ---------------------------------------------------------------------------
# Vitais — vitals.onnx × datasets de vitais
# ---------------------------------------------------------------------------

def benchmark_vitals(
    cfg: Config, onnx_path: Path, log: RunLogger,
    vitals_datasets: Sequence[Dataset],
) -> dict:
    from torch.utils.data import ConcatDataset

    from trainers import _split

    session = _session(onnx_path)

    # Mesma composição e split do treino (train_vitals): concat na mesma
    # ordem → maybe_subset → random_split com a mesma semente; avalia só o val.
    dataset: Dataset = (
        vitals_datasets[0] if len(vitals_datasets) == 1
        else ConcatDataset(list(vitals_datasets))
    )
    dataset = maybe_subset(dataset, cfg.fast_dev_max_samples, cfg.fast_dev)
    _, val_ds = _split(dataset, cfg.train_frac, cfg.random_seed)

    preds: List[np.ndarray] = []
    trues: List[np.ndarray] = []
    latency = None
    for i in range(len(val_ds)):
        x, target = val_ds[i]
        feed = x.unsqueeze(0).numpy().astype(np.float32)
        if latency is None:
            latency = _measure_latency(session, feed)
        (out,) = session.run(None, {"csi": feed})
        preds.append(out[0])
        trues.append(np.asarray(target, dtype=np.float32))

    p = np.asarray(preds, dtype=np.float64)
    t = np.asarray(trues, dtype=np.float64)

    # Máscaras por canal: NaN no target = sem ground truth (ex.: NIST não
    # anota batimento) — fica fora de MAE/Pearson/Bland-Altman.
    def _finite(col: int):
        if not len(p):
            return p[:, :0], t[:, :0]
        m = np.isfinite(t[:, col])
        return p[m, col], t[m, col]

    def _mae_per_min(col: int) -> Optional[float]:
        a, b = _finite(col)
        return float(np.mean(np.abs(a - b)) * 60) if len(a) else None

    def _pearson(col: int) -> Optional[float]:
        a, b = _finite(col)
        if len(a) < 2 or float(np.std(a)) == 0.0 or float(np.std(b)) == 0.0:
            return None
        return float(np.corrcoef(a, b)[0, 1])

    bp, bt = _finite(0)
    hp, ht = _finite(1)
    result = {
        "samples": len(p),
        "mae_breathing_rpm": _mae_per_min(0),
        "mae_heart_bpm": _mae_per_min(1),
        "pearson_breathing": _pearson(0),
        "pearson_heart": _pearson(1),
        # Pares com ground truth (em por-minuto) p/ o Bland-Altman do relatório
        "breathing_pred_rpm": [round(float(v) * 60, 3) for v in bp],
        "breathing_true_rpm": [round(float(v) * 60, 3) for v in bt],
        "heart_pred_bpm": [round(float(v) * 60, 3) for v in hp],
        "heart_true_bpm": [round(float(v) * 60, 3) for v in ht],
        "latency_ms": latency,
    }
    log.info("Benchmark vitais", samples=result["samples"],
             mae_breathing_rpm=result["mae_breathing_rpm"],
             mae_heart_bpm=result["mae_heart_bpm"], latency_ms=latency)
    return result


# ---------------------------------------------------------------------------
# Entradas principais
# ---------------------------------------------------------------------------

def run_offline_benchmark(
    cfg: Config,
    run_dir: Path,
    log: RunLogger,
    vitals_datasets: Sequence[Dataset] = (),
) -> Dict[str, Optional[dict]]:
    """Avalia todos os ONNX presentes em *run_dir*. Modelos ausentes → None."""
    results: Dict[str, Optional[dict]] = {
        "presence": None, "occupancy": None, "vitals": None,
    }

    presence_onnx = run_dir / "presence.onnx"
    if presence_onnx.exists():
        try:
            results["presence"] = benchmark_presence(cfg, presence_onnx, log)
        except Exception as exc:
            log.error(f"Benchmark presença falhou: {exc}")

    occupancy_onnx = run_dir / "occupancy.onnx"
    if occupancy_onnx.exists():
        try:
            results["occupancy"] = benchmark_occupancy(cfg, occupancy_onnx, log)
        except Exception as exc:
            log.error(f"Benchmark ocupação falhou: {exc}")

    vitals_onnx = run_dir / "vitals.onnx"
    if vitals_onnx.exists() and vitals_datasets:
        try:
            results["vitals"] = benchmark_vitals(cfg, vitals_onnx, log,
                                                 vitals_datasets)
        except Exception as exc:
            log.error(f"Benchmark vitais falhou: {exc}")

    return results


def run_live_benchmark(*_args, **_kwargs) -> None:
    """Benchmark ao vivo com ESP32 — Fase 3 (requer hardware)."""
    raise NotImplementedError(
        "Benchmark ao vivo é Fase 3: requer ESP32 transmitindo CSI_DATA via "
        "UDP:5005 e o aethersense-server rodando. Use o benchmark offline."
    )
