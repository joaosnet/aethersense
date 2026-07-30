"""
pipeline.py — orquestrador do pipeline ML do AetherSense.

Executa, numa única run com artefatos consolidados:

    download → validate → train (3 modelos) → benchmark → report

Cada estágio é tolerante a dados ausentes: um dataset indisponível pula o
modelo correspondente com aviso, sem derrubar o pipeline (mesma filosofia do
notebook legado). Artefatos em ml/artifacts/runs/<run_id>/:

    config.json               configuração exata da run
    log.jsonl / log.txt       histórico completo (treinos dos 3 modelos)
    checkpoint_*.pt           best por modelo (+ periódicos)
    presence.onnx / occupancy.onnx / vitals.onnx (+ occupancy_metadata.json)
    benchmark.json            métricas offline de todos os modelos
    pipeline_summary.json     resumo consolidado (treino + benchmark)
    report.html               relatório final (Jinja2 + matplotlib)

Uso programático (TUI usa isto):
    from pipeline import run_pipeline
    summary = run_pipeline(cfg, skip_download=False)

Uso direto:
    uv run pipeline.py --fast-dev          # smoke: download amostras + tudo
    uv run pipeline.py                     # completo
    uv run pipeline.py --skip-download     # usa datasets já presentes
"""

from __future__ import annotations

import json
from datetime import datetime, timezone
from pathlib import Path
from typing import Dict, List, Optional, Sequence

from rich.console import Console
from torch.utils.data import Dataset

from config import Config
from logger import RunLogger

DEFAULT_MODELS = ("presence", "occupancy", "vitals")


# ---------------------------------------------------------------------------
# Estágio: download
# ---------------------------------------------------------------------------

def stage_download(
    cfg: Config, smoke: bool, console: Console,
    include_wipose: bool = False, include_mmfi: bool = False,
) -> bool:
    from download_datasets import (
        download_miningtool,
        download_mmfi,
        download_nist,
        download_wimans,
        download_wipose,
        download_zenodo,
    )

    ok = True
    ok &= download_zenodo(cfg.zenodo_dir, smoke, console)
    ok &= download_wimans(cfg.wimans_dir, smoke, 24, 4, console)
    ok &= download_miningtool(cfg.miningtool_dir, smoke, console)
    ok &= download_nist(cfg.nist_dir, smoke, console)
    ok &= download_mmfi(cfg.mmfi_dir, smoke, include_mmfi, console)
    if include_wipose:
        ok &= download_wipose(cfg.wipose_dir, smoke, console)
    return ok


# ---------------------------------------------------------------------------
# Estágio: validate — disponibilidade de cada dataset
# ---------------------------------------------------------------------------

def stage_validate(cfg: Config, log: RunLogger) -> Dict[str, bool]:
    from datasets import (
        CSIMiningToolDataset,
        MMFiDataset,
        NISTBreatheSmartDataset,
        WiMANSDataset,
        WiPoseDataset,
        ZenodoPresenceDataset,
    )

    probes = {
        "zenodo":     lambda: ZenodoPresenceDataset(cfg.zenodo_dir, "train")[0],
        "wimans":     lambda: WiMANSDataset(cfg.wimans_dir)[0],
        "miningtool": lambda: CSIMiningToolDataset(
            cfg.miningtool_dir, cfg.target_subcarriers, cfg.breathing_timesteps)[0],
        "nist":       lambda: NISTBreatheSmartDataset(
            cfg.nist_dir, cfg.target_subcarriers, cfg.breathing_timesteps)[0],
        "wipose":     lambda: WiPoseDataset(
            cfg.wipose_dir, cfg.target_subcarriers, cfg.target_timesteps,
            cfg.heatmap_size)[0],
        "mmfi":       lambda: MMFiDataset(
            cfg.mmfi_dir, cfg.target_subcarriers, cfg.target_timesteps,
            cfg.heatmap_size)[0],
    }
    availability: Dict[str, bool] = {}
    for name, probe in probes.items():
        try:
            probe()
            availability[name] = True
        except Exception as exc:
            availability[name] = False
            log.info(f"Dataset {name} indisponível", reason=str(exc)[:160])
    log.info("Disponibilidade de datasets", **availability)
    return availability


# ---------------------------------------------------------------------------
# Construção de datasets auxiliares
# ---------------------------------------------------------------------------

def _vitals_datasets(cfg: Config, availability: Dict[str, bool]) -> List[Dataset]:
    from datasets import CSIMiningToolDataset, NISTBreatheSmartDataset

    out: List[Dataset] = []
    if availability.get("miningtool"):
        out.append(CSIMiningToolDataset(
            cfg.miningtool_dir, cfg.target_subcarriers, cfg.breathing_timesteps))
    if availability.get("nist"):
        out.append(NISTBreatheSmartDataset(
            cfg.nist_dir, cfg.target_subcarriers, cfg.breathing_timesteps))
    return out


def _occupancy_aux(cfg: Config, availability: Dict[str, bool]) -> List[Dataset]:
    from datasets import MMFiDataset, WiPoseDataset, maybe_subset

    out: List[Dataset] = []
    if availability.get("wipose"):
        out.append(maybe_subset(
            WiPoseDataset(cfg.wipose_dir, cfg.target_subcarriers,
                          cfg.target_timesteps, cfg.heatmap_size),
            cfg.fast_dev_max_samples, cfg.fast_dev))
    if availability.get("mmfi"):
        out.append(maybe_subset(
            MMFiDataset(cfg.mmfi_dir, cfg.target_subcarriers,
                        cfg.target_timesteps, cfg.heatmap_size),
            cfg.fast_dev_max_samples, cfg.fast_dev))
    return out


# ---------------------------------------------------------------------------
# Estágio: train
# ---------------------------------------------------------------------------

def stage_train(
    cfg: Config,
    run_dir: Path,
    run_id: str,
    log: RunLogger,
    availability: Dict[str, bool],
    models: Sequence[str],
) -> dict:
    training: dict = {}

    if "presence" in models:
        if availability.get("zenodo"):
            from trainers import train_presence
            _, training["presence_cnn"] = train_presence(cfg, run_dir, run_id, log)
        else:
            training["presence_cnn"] = {"skipped": True}
            log.info("PresenceCNN pulado — Zenodo indisponível")

    if "occupancy" in models:
        if availability.get("wimans"):
            from train import export_onnx as export_occupancy
            from train import train as train_occupancy

            model, summary, conf_mat, per_class = train_occupancy(
                cfg, run_dir, run_id, log,
                extra_datasets=_occupancy_aux(cfg, availability) or None,
            )
            log.info("Starting ONNX export …")
            _, logit_diff, heat_diff = export_occupancy(model, cfg, run_dir, log)
            _write_occupancy_metadata(cfg, run_dir, run_id, logit_diff, heat_diff)
            training["occupancy_heatmap"] = {
                "best_val_loss": summary["best_val"],
                "best_epoch": summary["best_epoch"],
                "onnx_logit_max_abs_diff": logit_diff,
                "onnx_heatmap_max_abs_diff": heat_diff,
                "confusion_matrix": conf_mat,
            }
        else:
            training["occupancy_heatmap"] = {"skipped": True}
            log.info("OccupancyHeatmapNet pulado — WiMANS indisponível")

    if "vitals" in models:
        vitals_ds = _vitals_datasets(cfg, availability)
        if vitals_ds:
            from trainers import train_vitals
            _, training["vitals_net"] = train_vitals(
                cfg, run_dir, run_id, log, vitals_ds)
        else:
            training["vitals_net"] = {"skipped": True}
            log.info("VitalsNet pulado — nenhum dataset de vitais disponível")

    return training


def _write_occupancy_metadata(
    cfg: Config, run_dir: Path, run_id: str,
    logit_diff: float, heat_diff: float,
) -> None:
    metadata = {
        "model_name":    "aethersense_occupancy_heatmap",
        "created_at":    datetime.now(timezone.utc).isoformat(),
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
        json.dumps(metadata, indent=2))


# ---------------------------------------------------------------------------
# Pipeline completo
# ---------------------------------------------------------------------------

def run_pipeline(
    cfg: Config,
    models: Sequence[str] = DEFAULT_MODELS,
    skip_download: bool = False,
    include_wipose: bool = False,
    include_mmfi: bool = False,
) -> dict:
    """Executa o pipeline completo. Retorna o resumo consolidado."""
    import uuid

    run_id = (datetime.now(timezone.utc).strftime("%Y%m%d_%H%M%S")
              + "_" + uuid.uuid4().hex[:8])
    run_dir = cfg.output_dir / "runs" / run_id
    run_dir.mkdir(parents=True, exist_ok=True)
    cfg.save(run_dir / "config.json")

    console = Console()
    summary: dict = {"run_id": run_id, "run_dir": str(run_dir),
                     "fast_dev": cfg.fast_dev, "stages": {}}

    with RunLogger(run_dir) as log:
        log.info("AetherSense ML — pipeline iniciado")
        log.info(f"Run ID  : {run_id}")
        log.info(f"Modelos : {', '.join(models)}")
        log.info(f"Modo    : {'smoke (fast-dev)' if cfg.fast_dev else 'completo'}")

        # ── download ────────────────────────────────────────────────────────
        if skip_download:
            log.info("Download pulado (--skip-download)")
            summary["stages"]["download"] = "skipped"
        else:
            ok = stage_download(cfg, cfg.fast_dev, console,
                                include_wipose, include_mmfi)
            summary["stages"]["download"] = "ok" if ok else "partial"
            if not ok:
                log.error("Download teve falhas — seguindo com o disponível")

        # ── validate ────────────────────────────────────────────────────────
        availability = stage_validate(cfg, log)
        summary["stages"]["validate"] = availability

        # ── train ───────────────────────────────────────────────────────────
        training = stage_train(cfg, run_dir, run_id, log, availability, models)
        summary["training"] = training
        summary["stages"]["train"] = {
            k: ("skipped" if v.get("skipped") else "ok")
            for k, v in training.items()
        }

        # ── benchmark ───────────────────────────────────────────────────────
        from benchmark import run_offline_benchmark

        bench = run_offline_benchmark(
            cfg, run_dir, log,
            vitals_datasets=_vitals_datasets(cfg, availability),
        )
        (run_dir / "benchmark.json").write_text(json.dumps(bench, indent=2))
        summary["benchmark"] = bench
        summary["stages"]["benchmark"] = {
            k: ("ok" if v else "skipped") for k, v in bench.items()
        }

        # ── report ──────────────────────────────────────────────────────────
        (run_dir / "pipeline_summary.json").write_text(
            json.dumps(summary, indent=2, default=lambda o: str(o)))
        from report import generate_html_report

        report_path = generate_html_report(run_dir, training, bench)
        summary["report"] = str(report_path)
        summary["stages"]["report"] = "ok"

        log.info("─" * 60)
        log.info(f"Pipeline completo. Artefatos: {run_dir}")
        log.info(f"  Relatório : {report_path}")
        log.info(f"  Benchmark : {run_dir / 'benchmark.json'}")
        log.info(f"  Resumo    : {run_dir / 'pipeline_summary.json'}")

    return summary


# ---------------------------------------------------------------------------
# CLI
# ---------------------------------------------------------------------------

def main() -> None:
    import argparse

    parser = argparse.ArgumentParser(
        description="AetherSense ML — pipeline completo "
                    "(download → validate → train → benchmark → report)",
        formatter_class=argparse.ArgumentDefaultsHelpFormatter,
    )
    parser.add_argument("--fast-dev", action="store_true",
                        help="Smoke test: amostras pequenas, 1 época.")
    parser.add_argument("--skip-download", action="store_true",
                        help="Usa os datasets já presentes em datasets/.")
    parser.add_argument("--models", nargs="+", default=list(DEFAULT_MODELS),
                        choices=list(DEFAULT_MODELS),
                        help="Quais modelos treinar.")
    parser.add_argument("--include-wipose", action="store_true")
    parser.add_argument("--include-mmfi", action="store_true")
    parser.add_argument("--data-dir", type=str, default=None)
    parser.add_argument("--epochs", type=int, default=None,
                        help="Épocas do OccupancyHeatmapNet (com multi-janela "
                             "K>1, passadas efetivas = epochs × K).")
    args = parser.parse_args()

    cfg = Config(fast_dev=args.fast_dev)
    if args.data_dir:
        cfg.data_dir = Path(args.data_dir)
        cfg.__post_init__()
    if args.epochs is not None and not args.fast_dev:
        cfg.epochs = args.epochs

    summary = run_pipeline(
        cfg, models=args.models, skip_download=args.skip_download,
        include_wipose=args.include_wipose, include_mmfi=args.include_mmfi,
    )
    raise SystemExit(0 if summary["stages"].get("report") == "ok" else 1)


if __name__ == "__main__":
    main()
