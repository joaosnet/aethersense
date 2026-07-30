"""
config.py — Central configuration for the AetherSense ML pipeline.

Two temporal scales:
  - LOCATION: short window (50 timesteps @ 20 Hz = 2.5 s) → OccupancyHeatmapNet
  - BREATHING: long window (500 timesteps @ 20 Hz = 25 s)  → BreathingEstimator (DSP)

Datasets are downloaded on Thunder Compute, not stored locally.
See ml/THUNDER_COMPUTE.md for setup and download instructions.
"""

from __future__ import annotations

import argparse
import json
import sys
from dataclasses import asdict, dataclass, field
from pathlib import Path


_REPO_ROOT = Path(__file__).resolve().parent.parent   # aethersense/
_ML_DIR    = Path(__file__).resolve().parent           # aethersense/ml/


@dataclass
class Config:
    # ── Dataset paths ─────────────────────────────────────────────────────────
    data_dir:   Path = _REPO_ROOT / "datasets"
    wimans_dir: Path = field(init=False)
    zenodo_dir: Path = field(init=False)
    wipose_dir: Path = field(init=False)
    mmfi_dir:   Path = field(init=False)
    miningtool_dir: Path = field(init=False)
    nist_dir:   Path = field(init=False)

    # ── Artifact output ────────────────────────────────────────────────────────
    output_dir: Path = _ML_DIR / "artifacts"

    # ── CSI acquisition parameters ────────────────────────────────────────────
    fs: int = 20                       # CSI sampling rate (Hz) — configure ESP32 accordingly

    # ── Location model ────────────────────────────────────────────────────────
    target_subcarriers: int = 64
    location_timesteps: int = 50       # 50 / 20 Hz = 2.5 s window
    heatmap_size: tuple[int, int] = (20, 20)

    # WiMANS discrete location anchors (normalised x, y)
    location_anchors: dict[str, tuple[float, float]] = field(default_factory=lambda: {
        "a": (0.18, 0.18),
        "b": (0.82, 0.18),
        "c": (0.50, 0.50),
        "d": (0.18, 0.82),
        "e": (0.82, 0.82),
    })

    # ── Breathing estimator (DSP) ─────────────────────────────────────────────
    breathing_timesteps: int = 500     # 500 / 20 Hz = 25 s window
    breathing_band: tuple[float, float] = (0.1, 0.5)  # Hz  → 6–30 rpm
    breathing_filter_order: int = 4    # Butterworth bandpass order
    # Max persons for ICA separation. Beyond this, report global rate + uncertain flag.
    max_persons_ica: int = 5

    # ── Training ─────────────────────────────────────────────────────────────
    epochs:      int   = 40            # OccupancyHeatmapNet (WiMANS)
    presence_epochs: int = 8           # PresenceCNN (Zenodo)
    vitals_epochs:   int = 30          # VitalsNet (CSI-MiningTool + NIST)
    batch_size:  int   = 64
    learning_rate: float = 1e-3
    random_seed: int   = 42
    train_frac:  float = 0.80
    heatmap_loss_weight: float = 1.0
    count_loss_weight:   float = 0.5
    # Contagem é ordinal: além da cross-entropy, penaliza a distância entre a
    # contagem esperada (softmax · classes) e o alvo. 0 desativa.
    count_ordinal_weight: float = 0.5
    # Multi-janela no treino do WiMANS: cada gravação de ~3 s entra K vezes
    # por época; com augment=True cada entrada recebe um recorte temporal
    # aleatório diferente → K janelas distintas da mesma gravação. O split
    # treino/val continua por GRAVAÇÃO (sem vazamento). Com K>1, reduza
    # --epochs proporcionalmente (passadas efetivas = epochs × K).
    wimans_windows_per_sample: int = 3
    # Máximo de pessoas distinguidas pela cabeça de contagem; a última classe
    # significa "max_count ou mais". O WiMANS rotula até 5 pessoas simultâneas.
    max_count: int = 5
    # Pondera a cross-entropy de contagem pelo inverso da frequência de classe
    # (WiMANS é desbalanceado: muitas amostras de 1 pessoa, poucas de 0/5).
    use_class_weights: bool = True

    # num_workers: 0 on Windows (no fork), 4 on Linux/Thunder
    num_workers: int = field(init=False)

    # ── Checkpointing ─────────────────────────────────────────────────────────
    checkpoint_every_n_epochs: int = 50

    # ── Mode flags ────────────────────────────────────────────────────────────
    fast_dev: bool = False
    fast_dev_max_samples: int = 256

    # ── Resume ────────────────────────────────────────────────────────────────
    resume: Path | None = None

    def __post_init__(self) -> None:
        self.wimans_dir     = self.data_dir / "WiMANS"
        self.zenodo_dir     = self.data_dir / "zenodo_8021099"
        self.wipose_dir     = self.data_dir / "Wi-Pose"
        self.mmfi_dir       = self.data_dir / "MM-Fi"
        self.miningtool_dir = self.data_dir / "WiFi-CSI-MiningTool"
        self.nist_dir       = self.data_dir / "NIST-BreatheSmart"
        self.num_workers = 0 if sys.platform == "win32" else 4

        if self.fast_dev:
            self.epochs          = 1
            self.presence_epochs = 1
            self.vitals_epochs   = 1
            self.batch_size      = 16

    # ── Derived helpers ───────────────────────────────────────────────────────

    @property
    def target_timesteps(self) -> int:
        """Alias for location_timesteps — the model input time dimension.

        train.py / export_onnx refer to the model input as ``target_timesteps``;
        keep a single source of truth in ``location_timesteps``.
        """
        return self.location_timesteps

    @property
    def location_window_seconds(self) -> float:
        return self.location_timesteps / self.fs

    @property
    def breathing_window_seconds(self) -> float:
        return self.breathing_timesteps / self.fs

    @property
    def fft_resolution_hz(self) -> float:
        """Frequency resolution of breathing FFT (Hz)."""
        return self.fs / self.breathing_timesteps

    def to_dict(self) -> dict:
        d = asdict(self)
        for k, v in d.items():
            if isinstance(v, Path):
                d[k] = str(v)
        return d

    def save(self, path: Path) -> None:
        path.write_text(json.dumps(self.to_dict(), indent=2))

    @classmethod
    def from_args(cls) -> "Config":
        parser = argparse.ArgumentParser(
            description="AetherSense ML — trains OccupancyHeatmapNet end-to-end",
            formatter_class=argparse.ArgumentDefaultsHelpFormatter,
        )
        parser.add_argument("--fast-dev", action="store_true", default=False,
                            help="Smoke test: 1 epoch, small dataset subset.")
        parser.add_argument("--data-dir", type=Path, default=None)
        parser.add_argument("--output-dir", type=Path, default=None)
        parser.add_argument("--resume", type=Path, default=None)
        parser.add_argument("--epochs", type=int, default=None)
        parser.add_argument("--batch-size", type=int, default=None)
        parser.add_argument("--lr", type=float, default=None, dest="learning_rate")
        parser.add_argument("--fs", type=int, default=None,
                            help="CSI sampling rate in Hz (default: 20).")
        parser.add_argument("--checkpoint-every", type=int, default=50,
                            dest="checkpoint_every_n_epochs")

        args = parser.parse_args()
        cfg = cls()
        cfg.fast_dev = args.fast_dev
        cfg.resume   = args.resume
        cfg.checkpoint_every_n_epochs = args.checkpoint_every_n_epochs

        if args.data_dir   is not None:
            cfg.data_dir = args.data_dir
            cfg.__post_init__()          # re-deriva todos os dirs de datasets
        if args.output_dir is not None: cfg.output_dir = args.output_dir
        if args.epochs     is not None: cfg.epochs = args.epochs
        if args.batch_size is not None: cfg.batch_size = args.batch_size
        if args.learning_rate is not None: cfg.learning_rate = args.learning_rate
        if args.fs         is not None: cfg.fs = args.fs

        if cfg.fast_dev:
            cfg.epochs          = 1
            cfg.presence_epochs = 1
            cfg.vitals_epochs   = 1
            cfg.batch_size      = 16

        return cfg
