"""
preprocessing.py — CSI signal preprocessing utilities for AetherSense.

Extracted from the notebook and made standalone. All functions are pure
(no global state) and accept/return numpy arrays or Python primitives.
"""

from __future__ import annotations

from typing import List, Sequence, Tuple

import numpy as np


# ---------------------------------------------------------------------------
# Temporal alignment
# ---------------------------------------------------------------------------

def pad_or_trim_time(matrix: np.ndarray, target_t: int = 50) -> np.ndarray:
    """
    Ensure the time axis (axis=1) has exactly *target_t* steps.

    - If longer: keep the last *target_t* timesteps (most recent).
    - If shorter: zero-pad on the left (front of sequence).

    Args:
        matrix: shape (subcarriers, timesteps), float32
        target_t: desired number of timesteps

    Returns:
        matrix of shape (subcarriers, target_t)
    """
    t = matrix.shape[1]
    if t > target_t:
        return matrix[:, -target_t:]
    if t < target_t:
        pad = np.zeros((matrix.shape[0], target_t - t), dtype=matrix.dtype)
        return np.concatenate([pad, matrix], axis=1)
    return matrix


# ---------------------------------------------------------------------------
# Temporal resampling (binned mean)
# ---------------------------------------------------------------------------

def resample_time(matrix: np.ndarray, target_t: int = 50) -> np.ndarray:
    """
    Reduz o eixo do tempo (axis=1) para *target_t* bins por média de janelas
    contíguas que cobrem a SÉRIE INTEIRA.

    Diferente de pad_or_trim_time (que fatia o final), isto preserva a
    assinatura temporal completa — crítico para gravações longas como o
    WiMANS (~2835 pacotes/3 s): fatiar o final manteria só ~53 ms de sinal.
    Para t <= target_t devolve a matriz intacta (caller decide o padding).
    """
    t = matrix.shape[1]
    if t <= target_t:
        return matrix
    edges = np.linspace(0, t, target_t + 1).astype(int)
    out = np.empty((matrix.shape[0], target_t), dtype=matrix.dtype)
    for i in range(target_t):
        hi = max(edges[i + 1], edges[i] + 1)
        out[:, i] = matrix[:, edges[i]:hi].mean(axis=1)
    return out


# ---------------------------------------------------------------------------
# Subcarrier resampling
# ---------------------------------------------------------------------------

def resample_subcarriers(matrix: np.ndarray, target_s: int = 64) -> np.ndarray:
    """
    Linearly interpolate along the subcarrier axis (axis=0) to *target_s* bins.

    Args:
        matrix: shape (subcarriers, timesteps), float32
        target_s: desired number of subcarriers

    Returns:
        matrix of shape (target_s, timesteps)
    """
    if matrix.shape[0] == target_s:
        return matrix
    src_x = np.linspace(0.0, 1.0, matrix.shape[0], dtype=np.float32)
    dst_x = np.linspace(0.0, 1.0, target_s, dtype=np.float32)
    out = np.empty((target_s, matrix.shape[1]), dtype=np.float32)
    for t in range(matrix.shape[1]):
        out[:, t] = np.interp(dst_x, src_x, matrix[:, t]).astype(np.float32)
    return out


# ---------------------------------------------------------------------------
# Full CSI preprocessing pipeline
# ---------------------------------------------------------------------------

def csi_array_to_subcarrier_time(
    arr: np.ndarray,
    target_subcarriers: int = 64,
    target_timesteps: int = 50,
) -> np.ndarray:
    """
    Convert a raw CSI amplitude array (any shape) to a normalised
    (subcarriers × timesteps) float32 matrix ready for the model.

    Steps:
        1. NaN / Inf cleanup → 0.0
        2. Shape normalisation: identify subcarrier axis heuristically
        3. Subcarrier resampling to *target_subcarriers*
        4. Temporal align: séries mais longas são reamostradas por média de
           bins cobrindo a duração inteira (resample_time); séries mais
           curtas recebem zero-padding à esquerda. Entradas já no tamanho
           alvo (ex.: feed live do ESP32) passam intactas.
        5. Per-subcarrier Z-score normalisation (baseline subtraction)

    Args:
        arr: raw array from .npy or .mat file, any rank
        target_subcarriers: output subcarrier count (default 64)
        target_timesteps: output timestep count (default 50)

    Returns:
        float32 ndarray of shape (target_subcarriers, target_timesteps)
    """
    return finalize_window(
        csi_array_to_subcarrier_matrix(arr, target_subcarriers),
        target_timesteps,
    )


def csi_array_to_subcarrier_matrix(
    arr: np.ndarray,
    target_subcarriers: int = 64,
) -> np.ndarray:
    """Etapas 1–3 do pipeline: sanitização, normalização de eixos e
    reamostragem de subportadoras. Retorna (target_subcarriers, T) com o
    eixo do tempo INTEIRO — útil para recortes de augmentation/TTA antes da
    janela final."""
    # 1. Sanitise
    arr = np.nan_to_num(
        np.asarray(arr, dtype=np.float32),
        nan=0.0,
        posinf=0.0,
        neginf=0.0,
    )

    # Known subcarrier counts for 802.11 CSI (used to disambiguate axes)
    _KNOWN_SUBCARRIERS = (30, 52, 56, 64, 114, 128)

    # 2. Normalise shape to 2-D (subcarriers × time)
    if arr.ndim == 2:
        matrix = arr
        rows_known = matrix.shape[0] in _KNOWN_SUBCARRIERS
        cols_known = matrix.shape[1] in _KNOWN_SUBCARRIERS
        # Prefer the axis whose length matches a known subcarrier count.
        # Only fall back to the "longest axis is time" heuristic when neither
        # axis is a recognisable subcarrier count — otherwise the default
        # (64 subcarriers × 50 timesteps) would be wrongly transposed.
        if rows_known and not cols_known:
            pass                        # already (subcarriers, time)
        elif cols_known and not rows_known:
            matrix = matrix.T           # (time, subcarriers) → (subcarriers, time)
        elif not rows_known and not cols_known and matrix.shape[0] > matrix.shape[1]:
            matrix = matrix.T           # heuristic: the longer axis is time
    else:
        shape = arr.shape
        time_axis = int(np.argmax(shape))
        candidate_axes = [
            i for i, s in enumerate(shape)
            if s in _KNOWN_SUBCARRIERS
        ]
        sub_axis = (
            candidate_axes[0]
            if candidate_axes
            else int(np.argmin([abs(s - 64) for s in shape]))
        )
        if sub_axis == time_axis and len(shape) > 1:
            sub_axis = 1 if time_axis != 1 else 0
        moved = np.moveaxis(arr, [sub_axis, time_axis], [0, 1])
        reduce_axes = tuple(range(2, moved.ndim))
        matrix = moved.mean(axis=reduce_axes) if reduce_axes else moved

    # 3. Resample subcarriers
    return resample_subcarriers(matrix.astype(np.float32), target_subcarriers)


def finalize_window(matrix: np.ndarray, target_timesteps: int = 50) -> np.ndarray:
    """Etapas 4–5 do pipeline: alinhamento temporal (bin-mean para séries
    longas, zero-pad para curtas) e Z-score por subportadora — aplicado por
    janela, igual à semântica do feed live."""
    matrix = resample_time(matrix, target_timesteps)
    matrix = pad_or_trim_time(matrix, target_timesteps)

    baseline = matrix.mean(axis=1, keepdims=True)
    std = matrix.std(axis=1, keepdims=True) + 1e-6
    return ((matrix - baseline) / std).astype(np.float32)


# ---------------------------------------------------------------------------
# Gaussian heatmap generation
# ---------------------------------------------------------------------------

def gaussian_heatmap(
    points: Sequence[Tuple[float, float]],
    size: Tuple[int, int] = (20, 20),
    sigma: float = 1.5,
) -> np.ndarray:
    """
    Generate a 2-D Gaussian heatmap from normalised (x, y) person coordinates.

    Args:
        points: list of (x_norm, y_norm) tuples in [0, 1] × [0, 1]
        size: output heatmap (height, width)
        sigma: Gaussian spread in pixel units on the output grid

    Returns:
        float32 ndarray of shape *size*, values in [0, 1]
    """
    h, w = size
    yy, xx = np.mgrid[0:h, 0:w]
    heatmap = np.zeros((h, w), dtype=np.float32)
    for x_norm, y_norm in points:
        if not (np.isfinite(x_norm) and np.isfinite(y_norm)):
            continue
        cx = np.clip(x_norm, 0.0, 1.0) * (w - 1)
        cy = np.clip(y_norm, 0.0, 1.0) * (h - 1)
        blob = np.exp(-((xx - cx) ** 2 + (yy - cy) ** 2) / (2 * sigma ** 2))
        heatmap = np.maximum(heatmap, blob)
    return heatmap.astype(np.float32)


# ---------------------------------------------------------------------------
# Heatmap decoding (inference post-processing)
# ---------------------------------------------------------------------------

def heatmap_to_persons(
    heatmap: np.ndarray,
    threshold: float = 0.35,
    max_people: int = 8,
) -> List[dict]:
    """
    Decode a predicted heatmap into a list of person location estimates
    using iterative peak finding (non-maximum suppression style).

    Args:
        heatmap: float32 ndarray of shape (H, W), values in [0, 1]
        threshold: minimum confidence to register a person
        max_people: maximum number of peaks to extract

    Returns:
        list of dicts with keys: x (float, 0–1), y (float, 0–1), confidence (float)
    """
    hm = heatmap.copy()
    persons = []
    for _ in range(max_people):
        y, x = np.unravel_index(np.argmax(hm), hm.shape)
        conf = float(hm[y, x])
        if conf < threshold:
            break
        persons.append({
            "x": float(x / max(1, hm.shape[1] - 1)),
            "y": float(y / max(1, hm.shape[0] - 1)),
            "confidence": conf,
        })
        # Zero out 5×5 neighbourhood around peak
        y0, y1 = max(0, y - 2), min(hm.shape[0], y + 3)
        x0, x1 = max(0, x - 2), min(hm.shape[1], x + 3)
        hm[y0:y1, x0:x1] = 0.0
    return persons
