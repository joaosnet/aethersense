"""
checkpointing.py — Save and load training checkpoints for the AetherSense pipeline.

Checkpoint strategy:
  - Save whenever val_loss improves (best model).
  - Save a periodic snapshot every N epochs (configurable, default 50).
  - Each checkpoint is a self-contained .pt file with everything needed
    to resume on a different machine / cloud service.

Checkpoint file format:
    {
        "epoch":           int,
        "model_name":      str,      # "presence_cnn" | "occupancy_heatmap"
        "model_state":     dict,     # state_dict
        "optimizer_state": dict,
        "config":          dict,     # Config.to_dict()
        "metrics_history": list,     # list of per-epoch metric dicts
        "best_val_loss":   float,
        "run_id":          str,
        "saved_at":        str,      # ISO-8601 UTC
    }
"""

from __future__ import annotations

from datetime import datetime, timezone
from pathlib import Path
from typing import Any

import torch
import torch.nn as nn


# ---------------------------------------------------------------------------
# Save
# ---------------------------------------------------------------------------

def save_checkpoint(
    *,
    run_dir: Path,
    model: nn.Module,
    optimizer: torch.optim.Optimizer,
    epoch: int,
    model_name: str,
    config_dict: dict,
    metrics_history: list[dict],
    best_val_loss: float,
    run_id: str,
    tag: str = "",          # e.g. "best" or "epoch050"
) -> Path:
    """
    Persist a full checkpoint.

    Returns the path where the checkpoint was saved.
    """
    suffix = f"_{tag}" if tag else f"_epoch{epoch:04d}"
    filename = f"checkpoint_{model_name}{suffix}.pt"
    path = run_dir / filename

    payload = {
        "epoch":           epoch,
        "model_name":      model_name,
        "model_state":     model.state_dict(),
        "optimizer_state": optimizer.state_dict(),
        "config":          config_dict,
        "metrics_history": metrics_history,
        "best_val_loss":   best_val_loss,
        "run_id":          run_id,
        "saved_at":        datetime.now(timezone.utc).isoformat(),
    }
    torch.save(payload, path)
    return path


# ---------------------------------------------------------------------------
# Load
# ---------------------------------------------------------------------------

def load_checkpoint(
    path: Path,
    model: nn.Module,
    optimizer: torch.optim.Optimizer | None = None,
    device: torch.device | str = "cpu",
) -> dict[str, Any]:
    """
    Load a checkpoint into *model* (and optionally *optimizer*).

    Returns the full payload dict so callers can read epoch, metrics_history,
    best_val_loss, run_id, etc.
    """
    payload = torch.load(path, map_location=device, weights_only=False)
    model.load_state_dict(payload["model_state"])

    if optimizer is not None and "optimizer_state" in payload:
        optimizer.load_state_dict(payload["optimizer_state"])
        # Move optimizer state tensors to the correct device
        for state in optimizer.state.values():
            for k, v in state.items():
                if isinstance(v, torch.Tensor):
                    state[k] = v.to(device)

    return payload


# ---------------------------------------------------------------------------
# Checkpoint manager — tracks best val_loss per phase
# ---------------------------------------------------------------------------

class CheckpointManager:
    """
    Manages saving checkpoints for a single training phase.

    Usage:
        ckpt_mgr = CheckpointManager(
            run_dir=run_dir,
            model_name="occupancy_heatmap",
            config_dict=cfg.to_dict(),
            run_id=run_id,
            checkpoint_every=50,
        )

        # Inside training loop:
        ckpt_mgr.step(
            epoch=epoch,
            model=model,
            optimizer=optimizer,
            metrics={"val_loss": 0.042, "val_acc": 0.88},
            log=logger,
        )
    """

    def __init__(
        self,
        *,
        run_dir: Path,
        model_name: str,
        config_dict: dict,
        run_id: str,
        checkpoint_every: int = 50,
        lower_is_better: bool = True,    # for val_loss
        metric_key: str = "val_loss",
    ) -> None:
        self.run_dir = run_dir
        self.model_name = model_name
        self.config_dict = config_dict
        self.run_id = run_id
        self.checkpoint_every = checkpoint_every
        self.lower_is_better = lower_is_better
        self.metric_key = metric_key

        self.best_val: float = float("inf") if lower_is_better else float("-inf")
        self.best_epoch: int = -1
        self.metrics_history: list[dict] = []
        self.best_checkpoint_path: Path | None = None

    def step(
        self,
        *,
        epoch: int,
        model: nn.Module,
        optimizer: torch.optim.Optimizer,
        metrics: dict[str, Any],
        log: Any,   # RunLogger — imported lazily to avoid circular
    ) -> None:
        """
        Called at the end of each epoch.

        Saves a checkpoint if:
          (a) the monitored metric improved, OR
          (b) epoch % checkpoint_every == 0
        """
        self.metrics_history.append({"epoch": epoch, **metrics})
        current = metrics.get(self.metric_key, None)

        improved = False
        if current is not None:
            if self.lower_is_better and current < self.best_val:
                improved = True
            elif not self.lower_is_better and current > self.best_val:
                improved = True

        periodic = (self.checkpoint_every > 0) and (epoch % self.checkpoint_every == 0)

        if improved:
            self.best_val = current
            self.best_epoch = epoch
            path = save_checkpoint(
                run_dir=self.run_dir,
                model=model,
                optimizer=optimizer,
                epoch=epoch,
                model_name=self.model_name,
                config_dict=self.config_dict,
                metrics_history=self.metrics_history,
                best_val_loss=self.best_val,
                run_id=self.run_id,
                tag="best",
            )
            self.best_checkpoint_path = path
            log.checkpoint(path, epoch=epoch, reason="improved",
                           **{self.metric_key: current})

        if periodic and not improved:
            path = save_checkpoint(
                run_dir=self.run_dir,
                model=model,
                optimizer=optimizer,
                epoch=epoch,
                model_name=self.model_name,
                config_dict=self.config_dict,
                metrics_history=self.metrics_history,
                best_val_loss=self.best_val,
                run_id=self.run_id,
                tag=f"epoch{epoch:04d}",
            )
            log.checkpoint(path, epoch=epoch, reason=f"periodic every {self.checkpoint_every}")

    def summary(self) -> dict:
        return {
            "best_val": self.best_val,
            "best_epoch": self.best_epoch,
            "best_checkpoint": str(self.best_checkpoint_path or ""),
            "total_epochs": len(self.metrics_history),
        }
