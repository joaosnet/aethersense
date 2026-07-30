"""
logger.py — Structured dual logging for AetherSense ML pipeline.

Produces two outputs simultaneously:
  1. Rich console — colourful, human-readable, works in background (nohup).
  2. JSON Lines file — machine-readable event log for TCC documentation.
  3. Plain text file — full transcript for attachment to TCC report.

Usage:
    from logger import RunLogger
    log = RunLogger(run_dir=Path("artifacts/runs/20260602_..."))
    log.info("Training started", phase="wimans", device="cuda")
    log.metric(epoch=1, loss=0.042, val_acc=0.721)
"""

from __future__ import annotations

import json
import sys
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

from rich.console import Console
from rich.text import Text
from rich.theme import Theme


# ---------------------------------------------------------------------------
# Theme — consistent colours for background-safe output
# ---------------------------------------------------------------------------

_THEME = Theme({
    "info":    "bold cyan",
    "warning": "bold yellow",
    "error":   "bold red",
    "metric":  "bold green",
    "phase":   "bold magenta",
    "dim":     "dim white",
})


class RunLogger:
    """
    Writes log events to:
      - Rich console (stdout) with colours
      - <run_dir>/log.jsonl  (one JSON object per line)
      - <run_dir>/log.txt    (plain text, no ANSI codes)

    All events carry a UTC ISO-8601 timestamp.
    """

    def __init__(self, run_dir: Path) -> None:
        self.run_dir = run_dir
        self.run_dir.mkdir(parents=True, exist_ok=True)

        self._jsonl_path = run_dir / "log.jsonl"
        self._txt_path   = run_dir / "log.txt"

        # Consoles do Windows usam cp1252 por padrão e quebram ao imprimir
        # glifos box-drawing / em-dash / ▶ emitidos pelo Rich (UnicodeEncodeError).
        # Forçar UTF-8 garante que smoke tests locais no Windows não morram no
        # resumo final. No-op no Linux/Thunder (já UTF-8).
        for _stream in (sys.stdout, sys.stderr):
            try:
                _stream.reconfigure(encoding="utf-8", errors="replace")
            except (AttributeError, ValueError):
                pass

        # Rich console — auto-detects if stdout is a TTY; falls back to plain
        # text when running under nohup/redirected output.
        # legacy_windows=False evita o renderizador Win32 (que reencoda em cp1252).
        self._console = Console(
            theme=_THEME,
            highlight=False,
            markup=True,
            force_terminal=False,   # respect actual TTY state
            legacy_windows=False,
        )
        # Separate console for the plain-text file (no ANSI)
        self._txt_file = self._txt_path.open("a", encoding="utf-8")
        self._file_console = Console(
            file=self._txt_file,
            theme=_THEME,
            highlight=False,
            markup=False,
            no_color=True,
            force_terminal=False,
        )

        self._jsonl_file = self._jsonl_path.open("a", encoding="utf-8")

    # ------------------------------------------------------------------
    # Public API
    # ------------------------------------------------------------------

    def info(self, message: str, **fields: Any) -> None:
        self._log("INFO", message, "info", **fields)

    def warning(self, message: str, **fields: Any) -> None:
        self._log("WARNING", message, "warning", **fields)

    def error(self, message: str, **fields: Any) -> None:
        self._log("ERROR", message, "error", **fields)

    def metric(self, **fields: Any) -> None:
        """Log a metrics snapshot (epoch, loss, val_acc, …)."""
        # Build a readable summary line
        parts = [f"{k}={_fmt(v)}" for k, v in fields.items()]
        self._log("METRIC", "  ".join(parts), "metric", **fields)

    def phase(self, name: str, **fields: Any) -> None:
        """Log the start of a training phase."""
        self._log("PHASE", f"▶  {name}", "phase", phase=name, **fields)

    def checkpoint(self, path: Path, **fields: Any) -> None:
        self._log("CHECKPOINT", f"Saved: {path}", "info", path=str(path), **fields)

    def close(self) -> None:
        self._jsonl_file.close()
        self._txt_file.close()

    def __enter__(self) -> "RunLogger":
        return self

    def __exit__(self, *_: Any) -> None:
        self.close()

    # ------------------------------------------------------------------
    # Internal helpers
    # ------------------------------------------------------------------

    def _now(self) -> str:
        return datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")

    def _log(self, level: str, message: str, style: str, **fields: Any) -> None:
        ts = self._now()
        # --- Console (rich) ---
        ts_text = f"[dim]{ts}[/dim]"
        level_text = f"[{style}]{level:<10}[/{style}]"
        self._console.print(f"{ts_text} {level_text} {message}")

        # --- Plain text file ---
        self._file_console.print(f"{ts} {level:<10} {message}")
        if fields:
            field_str = "  " + "  ".join(f"{k}={_fmt(v)}" for k, v in fields.items())
            self._file_console.print(field_str)
        self._txt_file.flush()

        # --- JSON lines ---
        event = {"ts": ts, "level": level, "msg": message, **fields}
        # Ensure all values are JSON-serialisable
        event = _make_serialisable(event)
        self._jsonl_file.write(json.dumps(event, ensure_ascii=False) + "\n")
        self._jsonl_file.flush()


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

def _fmt(v: Any) -> str:
    if isinstance(v, float):
        return f"{v:.4f}"
    return str(v)


def _make_serialisable(obj: Any) -> Any:
    """Recursively convert non-JSON-serialisable types."""
    if isinstance(obj, dict):
        return {k: _make_serialisable(v) for k, v in obj.items()}
    if isinstance(obj, (list, tuple)):
        return [_make_serialisable(x) for x in obj]
    if isinstance(obj, Path):
        return str(obj)
    if isinstance(obj, float) and (obj != obj):  # NaN
        return None
    return obj
