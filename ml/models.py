"""
models.py — PyTorch model definitions for AetherSense occupancy detection.

Models:
  - PresenceCNN         — binary presence classifier (Zenodo dataset)
  - OccupancyHeatmapNet — U-Net for multi-person count + location heatmap (WiMANS)
  - VitalsNet           — 1D CNN + LSTM for breathing/heart rate (CSI-MiningTool, NIST)
"""

from __future__ import annotations

import torch
import torch.nn as nn
import torch.nn.functional as F


# ---------------------------------------------------------------------------
# PresenceCNN — lightweight binary classifier
# ---------------------------------------------------------------------------

class PresenceCNN(nn.Module):
    """
    Small 3-layer CNN for binary presence detection.

    Input:  (B, 1, 64, 64)  — grayscale Zenodo PNG
    Output: (B, 2)           — logits for [empty, occupied]
    """

    def __init__(self) -> None:
        super().__init__()
        self.net = nn.Sequential(
            nn.Conv2d(1, 16, 3, padding=1), nn.ReLU(inplace=True), nn.MaxPool2d(2),
            nn.Conv2d(16, 32, 3, padding=1), nn.ReLU(inplace=True), nn.MaxPool2d(2),
            nn.Conv2d(32, 64, 3, padding=1), nn.ReLU(inplace=True), nn.AdaptiveAvgPool2d(1),
        )
        self.fc = nn.Linear(64, 2)

    def forward(self, x: torch.Tensor) -> torch.Tensor:
        return self.fc(self.net(x).flatten(1))


# ---------------------------------------------------------------------------
# ConvBlock — double conv + BatchNorm (U-Net building block)
# ---------------------------------------------------------------------------

class ConvBlock(nn.Module):
    """Two consecutive Conv2d → BatchNorm2d → ReLU layers."""

    def __init__(self, in_ch: int, out_ch: int) -> None:
        super().__init__()
        self.block = nn.Sequential(
            nn.Conv2d(in_ch, out_ch, 3, padding=1),
            nn.BatchNorm2d(out_ch),
            nn.ReLU(inplace=True),
            nn.Conv2d(out_ch, out_ch, 3, padding=1),
            nn.BatchNorm2d(out_ch),
            nn.ReLU(inplace=True),
        )

    def forward(self, x: torch.Tensor) -> torch.Tensor:
        return self.block(x)


# ---------------------------------------------------------------------------
# OccupancyHeatmapNet — main model (≈ 482k params)
# ---------------------------------------------------------------------------

class OccupancyHeatmapNet(nn.Module):
    """
    U-Net-style encoder-decoder with dual output heads:
      - count_logits : (B, max_count + 1) — classes: 0, 1, …, max_count(+)
      - heatmap      : (B, 1, H, W)       — Gaussian location heatmap (sigmoid)

    Input:  (B, 1, subcarriers, timesteps)  e.g. (B, 1, 64, 50)
    Outputs: tuple(count_logits, heatmap)

    Architecture:
        Encoder:    enc1(1→16) → enc2(16→32) → enc3(32→64) → bottleneck(64→128)
        Count head: AdaptiveAvgPool(bottleneck) → Linear(128→64) → Linear(64→C)
        Decoder:    up2(128→64) + skip(enc3) → dec2(128→64)
                    up1(64→32) + skip(enc2) → dec1(64→32)
                    heat conv(32→1) → sigmoid → interpolate to heatmap_size

    `max_count` is the maximum number of people the count head distinguishes;
    the last class doubles as "max_count or more". The number of output
    classes is therefore ``max_count + 1``.
    """

    def __init__(
        self,
        heatmap_size: tuple[int, int] = (20, 20),
        max_count: int = 5,
    ) -> None:
        super().__init__()
        self.heatmap_size = heatmap_size
        self.max_count = max_count
        num_classes = max_count + 1

        # Encoder
        self.enc1 = ConvBlock(1, 16)
        self.enc2 = ConvBlock(16, 32)
        self.enc3 = ConvBlock(32, 64)
        self.bottleneck = ConvBlock(64, 128)

        # Decoder
        self.up2 = nn.ConvTranspose2d(128, 64, kernel_size=2, stride=2)
        self.dec2 = ConvBlock(128, 64)     # 64 (up) + 64 (skip enc3) = 128 in
        self.up1 = nn.ConvTranspose2d(64, 32, kernel_size=2, stride=2)
        self.dec1 = ConvBlock(64, 32)      # 32 (up) + 32 (skip enc2) = 64 in

        # Output heads
        self.heat = nn.Conv2d(32, 1, kernel_size=1)
        self.count_head = nn.Sequential(
            nn.AdaptiveAvgPool2d(1),
            nn.Flatten(),
            nn.Linear(128, 64),
            nn.ReLU(inplace=True),
            nn.Dropout(0.3),
            nn.Linear(64, num_classes),
        )

    def forward(
        self, x: torch.Tensor
    ) -> tuple[torch.Tensor, torch.Tensor]:
        # Encode
        e1 = self.enc1(x)
        e2 = self.enc2(F.max_pool2d(e1, 2))
        e3 = self.enc3(F.max_pool2d(e2, 2))
        b  = self.bottleneck(F.max_pool2d(e3, 2))

        # Count head (from bottleneck)
        count_logits = self.count_head(b)

        # Decode
        d2 = self.up2(b)
        d2 = F.interpolate(d2, size=e3.shape[-2:], mode="bilinear", align_corners=False)
        d2 = self.dec2(torch.cat([d2, e3], dim=1))

        d1 = self.up1(d2)
        d1 = F.interpolate(d1, size=e2.shape[-2:], mode="bilinear", align_corners=False)
        d1 = self.dec1(torch.cat([d1, e2], dim=1))

        # Heatmap head
        heatmap = torch.sigmoid(
            F.interpolate(
                self.heat(d1),
                size=self.heatmap_size,
                mode="bilinear",
                align_corners=False,
            )
        )
        return count_logits, heatmap


# ---------------------------------------------------------------------------
# VitalsNet — breathing + heart rate estimator
# ---------------------------------------------------------------------------

class VitalsNet(nn.Module):
    """
    1D CNN (temporal) + LSTM for vital-sign estimation from CSI.

    Input:  (B, 1, subcarriers, timesteps)  e.g. (B, 1, 64, 500)
    Output: (B, 2) — [breathing_hz, heart_hz]

    The subcarrier axis becomes the Conv1d channel axis; convolutions slide
    over time only (vital signs are periodic in time, not across subcarriers).
    The LSTM summarises the filtered sequence and the head regresses the two
    frequencies, each squashed into its physiological band via scaled sigmoid:
        breathing: 0.05–0.70 Hz (3–42 rpm)
        heart:     0.60–3.50 Hz (36–210 bpm)
    Constraining the output keeps training stable on small datasets and means
    downstream consumers (Rust server) never see impossible frequencies.

    The temporal length is flexible (≥ 8 timesteps); export ONNX with the
    window the server will use (cfg.breathing_timesteps).
    """

    BREATHING_BAND_HZ: tuple[float, float] = (0.05, 0.70)
    HEART_BAND_HZ:     tuple[float, float] = (0.60, 3.50)

    def __init__(self, subcarriers: int = 64, lstm_hidden: int = 96) -> None:
        super().__init__()
        self.subcarriers = subcarriers

        self.temporal = nn.Sequential(
            nn.Conv1d(subcarriers, 64, kernel_size=7, padding=3),
            nn.BatchNorm1d(64),
            nn.ReLU(inplace=True),
            nn.MaxPool1d(2),
            nn.Conv1d(64, 96, kernel_size=5, padding=2),
            nn.BatchNorm1d(96),
            nn.ReLU(inplace=True),
            nn.MaxPool1d(2),
            nn.Conv1d(96, 128, kernel_size=3, padding=1),
            nn.BatchNorm1d(128),
            nn.ReLU(inplace=True),
        )
        self.lstm = nn.LSTM(128, lstm_hidden, batch_first=True)
        self.head = nn.Sequential(
            nn.Linear(lstm_hidden, 64),
            nn.ReLU(inplace=True),
            nn.Dropout(0.3),
            nn.Linear(64, 2),
        )

    def forward(self, x: torch.Tensor) -> torch.Tensor:
        # (B, 1, S, T) → (B, S, T): subcarriers viram canais do Conv1d
        feats = self.temporal(x.squeeze(1))          # (B, 128, T/4)
        seq, _ = self.lstm(feats.permute(0, 2, 1))   # (B, T/4, hidden)
        raw = self.head(seq.mean(dim=1))             # (B, 2) — média temporal
                                                     # é mais robusta que o
                                                     # último passo do LSTM

        b_lo, b_hi = self.BREATHING_BAND_HZ
        h_lo, h_hi = self.HEART_BAND_HZ
        breathing = b_lo + torch.sigmoid(raw[:, 0]) * (b_hi - b_lo)
        heart     = h_lo + torch.sigmoid(raw[:, 1]) * (h_hi - h_lo)
        return torch.stack([breathing, heart], dim=1)


# ---------------------------------------------------------------------------
# Utility
# ---------------------------------------------------------------------------

def parameter_count(model: nn.Module) -> int:
    """Return the number of trainable parameters."""
    return sum(p.numel() for p in model.parameters() if p.requires_grad)
