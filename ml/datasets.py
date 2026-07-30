"""
datasets.py — PyTorch Dataset classes for AetherSense ML pipeline.

Datasets de ocupação (retornam (csi, count, heatmap)):
  - ZenodoPresenceDataset  (Zenodo 8021099 — PNG images + CSV labels; binário)
  - WiMANSDataset          (WiMANS — .npy amplitude files + annotation.csv)
  - WiPoseDataset          (Wi-Pose — .mat; auxiliar: 1 pessoa, heatmap central)
  - MMFiDataset            (MM-Fi — .mat CSIamp; auxiliar: 1 pessoa por amostra)

Datasets de vitais (retornam (csi, target_hz) com NaN no canal sem ground truth):
  - CSIMiningToolDataset   (WiFi-CSI-MiningTool — CSV bruto 90 amp + 90 fase)
  - NISTBreatheSmartDataset (NIST mds2-2963 — CSVs real/imag, bpm do manequim)

Each dataset is fully independent: it can be instantiated and used without
the other. Pass a Config object (or its individual fields) to customise paths.

Important: num_workers should be 0 on Windows (no fork support) and 4 on Linux.
The Config class sets this automatically.
"""

from __future__ import annotations

from pathlib import Path
from typing import Dict, List, Optional, Tuple

import numpy as np
import pandas as pd
import torch
from PIL import Image
from torch.utils.data import Dataset, Subset

from preprocessing import (
    csi_array_to_subcarrier_matrix,
    csi_array_to_subcarrier_time,
    finalize_window,
    gaussian_heatmap,
)


# ---------------------------------------------------------------------------
# Zenodo 8021099 — Binary presence (person / no person)
# ---------------------------------------------------------------------------

class ZenodoPresenceDataset(Dataset):
    """
    Loads CSI heatmap images (64×64 PNG) from Zenodo record 8021099.

    Label CSV format: two columns (image_index, label) — no header.
    Label 0 → empty room, 1+ → person present (binarised to 0/1).

    Expected directory layout:
        zenodo_dir/
            DA_LOS/
                trainLabels.csv
                testLabels.csv
                DA_LOS_0001.png
                ...
            DA_NLOS/
            DP_LOS/
            DP_NLOS/
    """

    # Dataset-level normalisation constants (empirical from Zenodo images)
    _MEAN: float = 0.447
    _STD:  float = 0.162

    def __init__(self, root: Path, split: str = "train") -> None:
        """
        Args:
            root: path to the Zenodo dataset directory
            split: "train" or "test"

        Usa TODAS as zonas (DA_LOS, DA_NLOS, DP_LOS, DP_NLOS) presentes e
        resolve cada PNG dentro da pasta do próprio CSV — os nomes (0.png,
        1.png, …) se repetem entre zonas, então um índice global por stem
        carregaria imagens da zona errada.
        """
        self.root = Path(root)
        self.samples: List[Tuple[Path, int]] = []
        for label_csv in self._find_label_csvs(split):
            zone_dir = label_csv.parent
            images: Dict[str, Path] = {
                p.stem: p for p in zone_dir.rglob("*.png")
            }
            labels = pd.read_csv(
                label_csv, header=None, names=["image_index", "label"]
            )
            for _, row in labels.iterrows():
                key = Path(str(row["image_index"])).stem
                path = images.get(key) or images.get(str(row["image_index"]))
                if path is not None:
                    binary = 0 if int(float(row["label"])) == 0 else 1
                    self.samples.append((path, binary))
        if not self.samples:
            raise FileNotFoundError(
                f"No Zenodo samples for split={split!r} under {self.root}"
            )

    def __len__(self) -> int:
        return len(self.samples)

    def __getitem__(self, idx: int) -> Tuple[torch.Tensor, torch.Tensor]:
        image_path, label = self.samples[idx]
        img = Image.open(image_path).convert("L").resize((64, 64))
        arr = np.asarray(img, dtype=np.float32) / 255.0
        arr = (arr - self._MEAN) / self._STD
        return (
            torch.from_numpy(arr).unsqueeze(0),         # (1, 64, 64)
            torch.tensor(label, dtype=torch.long),
        )

    # ------------------------------------------------------------------

    def _find_label_csvs(self, split: str) -> List[Path]:
        candidates = sorted(
            set(self.root.rglob(f"{split}Labels.csv"))
            | set(self.root.rglob(f"{split}_labels.csv"))
        )
        if not candidates:
            raise FileNotFoundError(
                f"No Zenodo label CSV found for split={split!r} under {self.root}"
            )
        return candidates


# ---------------------------------------------------------------------------
# WiMANS — Multi-person count + location heatmap
# ---------------------------------------------------------------------------

class WiMANSDataset(Dataset):
    """
    Loads CSI amplitude data (.npy) and annotations from the WiMANS dataset.

    Expected directory layout:
        wimans_dir/
            annotation.csv
            wifi_csi/
                amp/
                    act_1_1.npy
                    act_1_2.npy
                    ...

    Each sample returns a 3-tuple:
        (csi_tensor, count_tensor, heatmap_tensor)

        csi_tensor:    float32, shape (1, target_subcarriers, target_timesteps)
        count_tensor:  long,    scalar  — number of people (clamped to [0, 3])
        heatmap_tensor: float32, shape (1, H, W)
    """

    # Discrete location labels → normalised (x, y) anchors
    _DEFAULT_ANCHORS: Dict[str, Tuple[float, float]] = {
        "a": (0.18, 0.18),
        "b": (0.82, 0.18),
        "c": (0.50, 0.50),
        "d": (0.18, 0.82),
        "e": (0.82, 0.82),
    }

    # Augmentation (somente com augment=True): recorte temporal aleatório,
    # ruído gaussiano pós-z-score e máscara de subportadoras.
    _AUG_CROP_MIN_FRAC = 0.7
    _AUG_NOISE_STD = 0.05
    _AUG_MAX_MASKED_SUBCARRIERS = 6

    # Janelas determinísticas de TTA (frações lo..hi do tempo total):
    # janela cheia + duas sub-janelas de 75% deslocadas.
    _TTA_SPANS: Tuple[Tuple[float, float], ...] = (
        (0.0, 1.0), (0.0, 0.75), (0.25, 1.0),
    )

    def __init__(
        self,
        root: Path,
        target_subcarriers: int = 64,
        target_timesteps: int = 50,
        heatmap_size: Tuple[int, int] = (20, 20),
        location_anchors: Optional[Dict[str, Tuple[float, float]]] = None,
        max_count: int = 5,
        augment: bool = False,
    ) -> None:
        self.root = Path(root)
        self.target_subcarriers = target_subcarriers
        self.target_timesteps = target_timesteps
        self.heatmap_size = heatmap_size
        self.location_anchors = location_anchors or self._DEFAULT_ANCHORS
        self.max_count = max_count
        self.augment = augment
        self._rng: Optional[np.random.Generator] = None

        ann_path = next(self.root.glob("**/annotation.csv"), None)
        if ann_path is None:
            raise FileNotFoundError(
                f"No WiMANS annotation.csv found under {self.root}"
            )
        self.annotations = pd.read_csv(ann_path, dtype=str)

        self.amp_dir: Path = (
            next(self.root.glob("**/wifi_csi/amp"), None)
            or next(self.root.glob("**/amp"), None)
        )
        if self.amp_dir is None:
            raise FileNotFoundError(
                f"No WiMANS CSI amplitude folder found under {self.root}"
            )

        self._location_cols: List[str] = [
            c for c in (f"user_{i}_location" for i in range(1, 7))
            if c in self.annotations.columns
        ]

    def __len__(self) -> int:
        return len(self.annotations)

    def count_labels(self) -> List[int]:
        """Contagem (clampada a max_count) de todas as amostras.

        Lê apenas o CSV de anotações (não carrega os .npy), portanto é barato
        e adequado para calcular pesos de classe antes do treino.
        """
        return [
            min(self._count(self.annotations.iloc[i]), self.max_count)
            for i in range(len(self.annotations))
        ]

    def _full_matrix(self, idx: int) -> np.ndarray:
        """(target_subcarriers, T_completo) — pré-janela, pré-z-score."""
        row = self.annotations.iloc[idx]
        arr = np.load(self.amp_dir / f"{self._label(row)}.npy")
        return csi_array_to_subcarrier_matrix(arr, self.target_subcarriers)

    def _worker_rng(self) -> np.random.Generator:
        # Workers do DataLoader são forks: semear lazy por worker evita que
        # todos os processos gerem o mesmo ruído.
        if self._rng is None:
            info = torch.utils.data.get_worker_info()
            seed = (info.seed if info is not None else torch.initial_seed())
            self._rng = np.random.default_rng(seed % (2 ** 32))
        return self._rng

    def tta_windows(self, idx: int) -> List[torch.Tensor]:
        """Janelas determinísticas para test-time augmentation (votação)."""
        full = self._full_matrix(idx)
        t = full.shape[1]
        windows = []
        for lo, hi in self._TTA_SPANS:
            a = int(t * lo)
            b = max(int(t * hi), a + 1)
            matrix = finalize_window(full[:, a:b], self.target_timesteps)
            windows.append(torch.from_numpy(matrix).unsqueeze(0))
        return windows

    def __getitem__(
        self, idx: int
    ) -> Tuple[torch.Tensor, torch.Tensor, torch.Tensor]:
        row = self.annotations.iloc[idx]
        full = self._full_matrix(idx)

        if self.augment:
            rng = self._worker_rng()
            t = full.shape[1]
            if t > 2 * self.target_timesteps:
                frac = rng.uniform(self._AUG_CROP_MIN_FRAC, 1.0)
                width = max(int(t * frac), self.target_timesteps)
                start = int(rng.integers(0, t - width + 1))
                full = full[:, start:start + width]

        matrix = finalize_window(full, self.target_timesteps)

        if self.augment:
            rng = self._worker_rng()
            matrix = matrix + rng.normal(
                0.0, self._AUG_NOISE_STD, matrix.shape
            ).astype(np.float32)
            n_mask = int(rng.integers(0, self._AUG_MAX_MASKED_SUBCARRIERS + 1))
            if n_mask:
                rows = rng.choice(matrix.shape[0], size=n_mask, replace=False)
                matrix[rows] = 0.0

        # Count label (clamped to max_count)
        count = min(self._count(row), self.max_count)

        # Location heatmap
        points = self._locations(row)
        heatmap = gaussian_heatmap(points, size=self.heatmap_size)

        return (
            torch.from_numpy(matrix).unsqueeze(0),          # (1, S, T)
            torch.tensor(count, dtype=torch.long),
            torch.from_numpy(heatmap).unsqueeze(0),          # (1, H, W)
        )

    # ------------------------------------------------------------------
    # Private helpers

    def _label(self, row: pd.Series) -> str:
        for col in ("label", "sample_id", "id"):
            val = row.get(col)
            if isinstance(val, str) and val.strip():
                return val
        raise KeyError(f"WiMANS row has no label/sample_id/id column: {dict(row)}")

    def _count(self, row: pd.Series) -> int:
        for col in ("number_of_users", "user_count"):
            val = row.get(col)
            if val is not None and pd.notna(val):
                return int(float(val))
        return sum(
            1 for col in self._location_cols
            if str(row.get(col, "nan")).lower() != "nan"
        )

    def _locations(self, row: pd.Series) -> List[Tuple[float, float]]:
        # Prefer explicit numeric coordinates
        for x_col, y_col in (("x", "y"), ("loc_x", "loc_y"), ("location_x", "location_y")):
            xv, yv = row.get(x_col), row.get(y_col)
            if xv is not None and yv is not None and pd.notna(xv) and pd.notna(yv):
                return [(float(xv), float(yv))]

        # Fall back to discrete anchor labels
        points: List[Tuple[float, float]] = []
        for col in self._location_cols:
            lbl = str(row.get(col, "nan")).strip().lower()
            if lbl in self.location_anchors:
                points.append(self.location_anchors[lbl])
        return points


# ---------------------------------------------------------------------------
# Wi-Pose — auxiliar de ocupação (1 sujeito por amostra)
# ---------------------------------------------------------------------------

class WiPoseDataset(Dataset):
    """
    Carrega arquivos .mat do Wi-Pose como dados auxiliares de ocupação.

    Cada .mat traz CSI de UM sujeito executando poses (formato nominal
    5×30×3×3 — frames × subcarriers × TX × RX). Como o Wi-Pose não anota
    posição na sala, a supervisão é fraca: count = 1 e heatmap centrado
    em (0.5, 0.5). Útil apenas como pré-treino/regularização do encoder.
    """

    def __init__(
        self,
        root: Path,
        target_subcarriers: int = 64,
        target_timesteps: int = 50,
        heatmap_size: Tuple[int, int] = (20, 20),
    ) -> None:
        self.root = Path(root)
        self.target_subcarriers = target_subcarriers
        self.target_timesteps = target_timesteps
        self.heatmap_size = heatmap_size
        self.files: List[Path] = sorted(self.root.rglob("*.mat"))
        if not self.files:
            raise FileNotFoundError(f"No Wi-Pose .mat files under {self.root}")

    def __len__(self) -> int:
        return len(self.files)

    @staticmethod
    def _first_array(mat: dict) -> np.ndarray:
        arrays = [
            v for k, v in mat.items()
            if not k.startswith("__") and isinstance(v, np.ndarray) and v.size > 1
        ]
        if not arrays:
            raise ValueError("Wi-Pose .mat sem array de dados")
        return max(arrays, key=lambda a: a.size)

    def __getitem__(
        self, idx: int
    ) -> Tuple[torch.Tensor, torch.Tensor, torch.Tensor]:
        import scipy.io as sio

        arr = np.abs(self._first_array(sio.loadmat(self.files[idx]))).astype(np.float32)
        # Formato nominal (frames, subc, tx, rx): reduz antenas e põe
        # subcarriers nas linhas antes do pipeline genérico.
        if arr.ndim == 4 and arr.shape[-2:] == (3, 3):
            arr = arr.mean(axis=(-2, -1)).T        # (subc, frames)
        matrix = csi_array_to_subcarrier_time(
            arr, self.target_subcarriers, self.target_timesteps
        )
        heatmap = gaussian_heatmap([(0.5, 0.5)], size=self.heatmap_size)
        return (
            torch.from_numpy(matrix).unsqueeze(0),
            torch.tensor(1, dtype=torch.long),
            torch.from_numpy(heatmap).unsqueeze(0),
        )


# ---------------------------------------------------------------------------
# MM-Fi — auxiliar de ocupação (1 sujeito por sequência)
# ---------------------------------------------------------------------------

class MMFiDataset(Dataset):
    """
    Carrega sequências WiFi-CSI do MM-Fi (E*/S*/A*/wifi-csi/frame*.mat).

    Cada frame .mat tem chave 'CSIamp' com shape (3, 114, 10) — antenas RX ×
    subcarriers × pacotes na janela de 100 ms. Uma amostra = uma sequência:
    concatena os primeiros *frames_per_sample* frames no eixo do tempo.

    Supervisão fraca (como Wi-Pose): count = 1, heatmap central — o MM-Fi
    anota pose em coordenadas de câmera, não posição na sala.

    Licença CC BY-NC 4.0 — uso acadêmico com citação.
    """

    def __init__(
        self,
        root: Path,
        target_subcarriers: int = 64,
        target_timesteps: int = 50,
        heatmap_size: Tuple[int, int] = (20, 20),
        frames_per_sample: int = 5,
    ) -> None:
        self.root = Path(root)
        self.target_subcarriers = target_subcarriers
        self.target_timesteps = target_timesteps
        self.heatmap_size = heatmap_size
        self.frames_per_sample = frames_per_sample
        self.sequences: List[Path] = sorted(
            p for p in self.root.rglob("wifi-csi") if p.is_dir()
        )
        if not self.sequences:
            raise FileNotFoundError(
                f"No MM-Fi wifi-csi sequences under {self.root} "
                "(o CSI do MM-Fi requer download de um E0x.zip — ver "
                "download_datasets.py --include-mmfi)"
            )

    def __len__(self) -> int:
        return len(self.sequences)

    def __getitem__(
        self, idx: int
    ) -> Tuple[torch.Tensor, torch.Tensor, torch.Tensor]:
        import scipy.io as sio

        frames = sorted(self.sequences[idx].glob("frame*.mat"))
        chunks = []
        for f in frames[: self.frames_per_sample]:
            amp = np.asarray(sio.loadmat(f)["CSIamp"], dtype=np.float32)
            chunks.append(amp.mean(axis=0))            # (114, 10) — média RX
        matrix = np.concatenate(chunks, axis=1)        # (114, 10·frames)
        matrix = csi_array_to_subcarrier_time(
            matrix, self.target_subcarriers, self.target_timesteps
        )
        heatmap = gaussian_heatmap([(0.5, 0.5)], size=self.heatmap_size)
        return (
            torch.from_numpy(matrix).unsqueeze(0),
            torch.tensor(1, dtype=torch.long),
            torch.from_numpy(heatmap).unsqueeze(0),
        )


# ---------------------------------------------------------------------------
# WiFi-CSI-MiningTool — vitais (respiração + batimento)
# ---------------------------------------------------------------------------

class CSIMiningToolDataset(Dataset):
    """
    Janelas de CSI bruto do WiFi-CSI-MiningTool com ground truth de vitais.

    Fontes (CSV sem header, ~25 Hz):
      - Breathing/S*_AMP_PHASE/s*_{R}BPM.csv — 181 colunas
        [timestamp, 90 amplitudes, 90 fases]; R ∈ {9,12,15,18,21} respirações
        por minuto vem do NOME do arquivo → target [R/60 Hz, NaN].
      - Heart-Rate/S*/S*_RAW.csv (mesmas 181 colunas) + HRReal.CSV
        (sequência de HR em BPM) → target [NaN, HR médio da janela / 60].

    Cada item: (csi (1, target_subcarriers, window_timesteps), target (2,)).
    NaN no canal sem ground truth — o treino usa L1 mascarada.
    """

    _AMP_COLS = slice(1, 91)    # 90 colunas de amplitude após o timestamp

    def __init__(
        self,
        root: Path,
        target_subcarriers: int = 64,
        window_timesteps: int = 500,
    ) -> None:
        self.root = Path(root)
        self.target_subcarriers = target_subcarriers
        self.window_timesteps = window_timesteps

        # Índice: (arquivo, índice da janela, [breathing_hz, heart_hz-fonte])
        self._index: List[Tuple[Path, int, float, Optional[Path]]] = []
        # Cache integral por arquivo: com DataLoader(shuffle=True) o acesso é
        # aleatório entre arquivos, então cache de 1 entrada re-parseia um CSV
        # de ~9 MB por item. O dataset completo em float32 ocupa <1 GB.
        self._cache: Dict[Path, np.ndarray] = {}

        import re

        for f in sorted(self.root.rglob("*BPM.csv")):
            m = re.search(r"(\d+)\s*BPM", f.name, re.IGNORECASE)
            if not m or "AccData" in f.name:
                continue
            rate_hz = float(m.group(1)) / 60.0
            for w in range(self._windows_in(f)):
                self._index.append((f, w, rate_hz, None))

        for raw in sorted(self.root.rglob("*_RAW.csv")):
            hr_file = next(
                (p for p in raw.parent.iterdir()
                 if p.name.lower() == "hrreal.csv"), None,
            )
            if hr_file is None:
                continue
            for w in range(self._windows_in(raw)):
                self._index.append((raw, w, float("nan"), hr_file))

        if not self._index:
            raise FileNotFoundError(
                f"No CSI-MiningTool CSV data under {self.root}"
            )

    def _windows_in(self, path: Path) -> int:
        # Conta linhas sem carregar o CSV inteiro em floats
        with path.open("rb") as fh:
            rows = sum(1 for _ in fh)
        return max(1, rows // self.window_timesteps)

    def _load_amp(self, path: Path) -> np.ndarray:
        amp = self._cache.get(path)
        if amp is None:
            data = pd.read_csv(path, header=None).to_numpy(dtype=np.float32)
            amp = data[:, self._AMP_COLS]          # (T, 90)
            self._cache[path] = amp
        return amp

    def __len__(self) -> int:
        return len(self._index)

    def __getitem__(self, idx: int) -> Tuple[torch.Tensor, torch.Tensor]:
        path, window, breathing_hz, hr_file = self._index[idx]
        amp = self._load_amp(path)
        start = window * self.window_timesteps
        chunk = amp[start: start + self.window_timesteps]    # (t, 90)
        matrix = csi_array_to_subcarrier_time(
            chunk.T, self.target_subcarriers, self.window_timesteps
        )

        heart_hz = float("nan")
        if hr_file is not None:
            hr = self._cache.get(hr_file)
            if hr is None:
                hr = pd.read_csv(hr_file, header=None).to_numpy(dtype=np.float32).ravel()
                self._cache[hr_file] = hr
            if len(hr):
                # Mapeia a fração temporal da janela na sequência de HR
                n_windows = max(1, len(amp) // self.window_timesteps)
                lo = int(len(hr) * window / n_windows)
                hi = max(lo + 1, int(len(hr) * (window + 1) / n_windows))
                heart_hz = float(np.mean(hr[lo:hi])) / 60.0

        target = torch.tensor([breathing_hz, heart_hz], dtype=torch.float32)
        return torch.from_numpy(matrix).unsqueeze(0), target


# ---------------------------------------------------------------------------
# NIST BreatheSmart — vitais (respiração de manequim robótico)
# ---------------------------------------------------------------------------

class NISTBreatheSmartDataset(Dataset):
    """
    Janelas de CSI do NIST BreatheSmart (mds2-2963).

    Cada gravação é um diretório config*/ com:
      - config*_csi_real_log.csv / config*_csi_imag_log.csv — N×1026 floats
        (3 TX × 3 RX × 114 subcarriers; colunas não usadas são zero)
      - config*.csv — condição do teste, campo `bpm` = taxa de respiração
        programada no manequim (ground truth)

    Amplitude = |real + j·imag|; colunas todas-zero são descartadas e o
    restante vira o eixo de subcarriers. Target: [bpm/60 Hz, NaN] — o NIST
    não tem ground truth de batimento cardíaco.
    """

    def __init__(
        self,
        root: Path,
        target_subcarriers: int = 64,
        window_timesteps: int = 500,
    ) -> None:
        self.root = Path(root)
        self.target_subcarriers = target_subcarriers
        self.window_timesteps = window_timesteps

        self._recordings: List[Tuple[Path, float]] = []
        # Cache integral por gravação (~2,4 MB cada, 343 no dataset completo):
        # sem ele, cada época re-parseia 2 CSVs por item.
        self._cache: Dict[Path, np.ndarray] = {}
        for real_csv in sorted(self.root.rglob("*_csi_real_log.csv")):
            bpm = self._bpm_from_config(real_csv.parent)
            if bpm is not None:
                self._recordings.append((real_csv, bpm / 60.0))
        if not self._recordings:
            raise FileNotFoundError(
                f"No NIST BreatheSmart recordings under {self.root}"
            )

    @staticmethod
    def _bpm_from_config(folder: Path) -> Optional[float]:
        for cfg in folder.glob("*.csv"):
            if "csi" in cfg.name.lower():
                continue
            try:
                df = pd.read_csv(cfg, header=None, dtype=str)
            except Exception:
                continue
            for _, row in df.iterrows():
                key = str(row.iloc[0]).strip().lower()
                if key == "bpm" and len(row) > 1:
                    try:
                        return float(row.iloc[1])
                    except (TypeError, ValueError):
                        return None
        return None

    def __len__(self) -> int:
        return len(self._recordings)

    def __getitem__(self, idx: int) -> Tuple[torch.Tensor, torch.Tensor]:
        real_csv, breathing_hz = self._recordings[idx]
        used = self._cache.get(real_csv)
        if used is None:
            imag_csv = real_csv.with_name(
                real_csv.name.replace("_real_", "_imag_"))
            real = pd.read_csv(real_csv, header=None).to_numpy(dtype=np.float32)
            imag = pd.read_csv(imag_csv, header=None).to_numpy(dtype=np.float32)
            amp = np.sqrt(real ** 2 + imag ** 2)           # (N, 1026)
            used = amp[:, np.any(amp != 0.0, axis=0)]      # descarta colunas zero
            if used.shape[1] == 0:
                used = amp
            self._cache[real_csv] = used
        matrix = csi_array_to_subcarrier_time(
            used[-self.window_timesteps:].T,
            self.target_subcarriers, self.window_timesteps,
        )
        target = torch.tensor([breathing_hz, float("nan")], dtype=torch.float32)
        return torch.from_numpy(matrix).unsqueeze(0), target


# ---------------------------------------------------------------------------
# Utility
# ---------------------------------------------------------------------------

def maybe_subset(dataset: Dataset, max_items: int, fast_dev: bool) -> Dataset:
    """Return a Subset of *max_items* when fast_dev is True."""
    if not fast_dev or len(dataset) <= max_items:
        return dataset
    return Subset(dataset, list(range(max_items)))
