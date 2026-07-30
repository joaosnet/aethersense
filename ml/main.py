"""
main.py — TUI interativo do pipeline ML do AetherSense.

Menu Rich que orquestra tudo sem decorar flags:

    [1] Smoke Test          — amostras pequenas, 1 época, 3 modelos, relatório
    [2] Pipeline Completo   — download integral + treino completo (GPU)
    [3] Baixar datasets     — só o download (smoke ou completo)
    [4] Treinar modelo      — um modelo individual (presence/occupancy/vitals)
    [5] Benchmark           — offline (datasets) ou ao vivo (ESP32, Fase 3)
    [6] Gerar relatório     — re-gera o report.html de uma run existente
    [0] Sair

Uso:
    cd ml/
    uv run main.py
"""

from __future__ import annotations

import sys
from pathlib import Path
from typing import Optional

from rich.console import Console
from rich.panel import Panel
from rich.prompt import Confirm, Prompt
from rich.table import Table

from config import Config

_ML_DIR = Path(__file__).resolve().parent


def _console() -> Console:
    for stream in (sys.stdout, sys.stderr):
        try:
            stream.reconfigure(encoding="utf-8", errors="replace")
        except (AttributeError, ValueError):
            pass
    return Console(highlight=False, legacy_windows=False)


# ---------------------------------------------------------------------------
# Seleção de runs existentes
# ---------------------------------------------------------------------------

def _runs_root() -> Path:
    return _ML_DIR / "artifacts" / "runs"


def _pick_run(console: Console, require_onnx: bool = False) -> Optional[Path]:
    runs = sorted(_runs_root().glob("*"), reverse=True) if _runs_root().exists() else []
    if require_onnx:
        runs = [r for r in runs if any(r.glob("*.onnx"))]
    if not runs:
        console.print("[yellow]Nenhuma run encontrada em ml/artifacts/runs/.[/yellow]")
        return None
    table = Table(title="Runs disponíveis (mais recentes primeiro)")
    table.add_column("#")
    table.add_column("run_id")
    table.add_column("artefatos")
    shown = runs[:8]
    for i, r in enumerate(shown, 1):
        arts = ", ".join(sorted(p.name for p in r.glob("*.onnx"))) or "—"
        table.add_row(str(i), r.name, arts)
    console.print(table)
    choice = Prompt.ask(
        "Run", choices=[str(i) for i in range(1, len(shown) + 1)], default="1")
    return shown[int(choice) - 1]


# ---------------------------------------------------------------------------
# Ações do menu
# ---------------------------------------------------------------------------

def action_smoke(console: Console) -> None:
    console.print(Panel(
        "Smoke test: baixa amostras pequenas de todos os datasets "
        "(Wi-Pose e CSI do MM-Fi pulados), treina 1 época dos 3 modelos, "
        "roda o benchmark offline e gera o relatório HTML.",
        title="[1] Smoke Test", border_style="cyan"))
    from pipeline import run_pipeline

    summary = run_pipeline(Config(fast_dev=True))
    console.print(f"\n[bold green]Smoke test concluído.[/bold green] "
                  f"Relatório: {summary.get('report')}")


def action_full(console: Console) -> None:
    console.print(Panel(
        "Pipeline completo: download integral (WiMANS ~19 GB + CSI-MiningTool "
        "~459 MB + NIST ~202 MB) e treino completo dos 3 modelos. "
        "Recomendado em GPU (Thunder Compute).",
        title="[2] Pipeline Completo", border_style="magenta"))
    import torch

    if not torch.cuda.is_available():
        console.print("[yellow]CUDA indisponível — o treino completo em CPU "
                      "pode levar dias.[/yellow]")
    if not Confirm.ask("Continuar?", default=False):
        return
    include_wipose = Confirm.ask("Incluir Wi-Pose (auxiliar)?", default=False)
    include_mmfi = Confirm.ask(
        "Incluir CSI do MM-Fi (~21 GB, CC BY-NC 4.0)?", default=False)
    from pipeline import run_pipeline

    summary = run_pipeline(Config(), include_wipose=include_wipose,
                           include_mmfi=include_mmfi)
    console.print(f"\n[bold green]Pipeline concluído.[/bold green] "
                  f"Relatório: {summary.get('report')}")


def action_download(console: Console) -> None:
    mode = Prompt.ask("Modo", choices=["smoke", "completo"], default="smoke")
    include_wipose = Confirm.ask("Incluir Wi-Pose?", default=False)
    include_mmfi = (mode == "completo") and Confirm.ask(
        "Incluir CSI do MM-Fi (~21 GB)?", default=False)
    from pipeline import stage_download

    cfg = Config(fast_dev=(mode == "smoke"))
    ok = stage_download(cfg, mode == "smoke", console,
                        include_wipose, include_mmfi)
    console.print("[bold green]Download concluído.[/bold green]" if ok
                  else "[bold red]Download teve falhas.[/bold red]")


def action_train_one(console: Console) -> None:
    model = Prompt.ask("Modelo",
                       choices=["presence", "occupancy", "vitals"],
                       default="occupancy")
    fast = Confirm.ask("Modo rápido (1 época, subset)?", default=True)
    from pipeline import run_pipeline

    summary = run_pipeline(Config(fast_dev=fast), models=[model],
                           skip_download=True)
    console.print(f"\n[bold green]Treino de {model} concluído.[/bold green] "
                  f"Relatório: {summary.get('report')}")


def action_benchmark(console: Console) -> None:
    mode = Prompt.ask("Benchmark", choices=["offline", "ao-vivo"],
                      default="offline")
    if mode == "ao-vivo":
        console.print(Panel(
            "Benchmark ao vivo é Fase 3: requer ESP32 transmitindo CSI_DATA "
            "via UDP:5005 e o aethersense-server rodando. Quando o hardware "
            "estiver disponível, esta opção compara as predições ML com a "
            "heurística atual em tempo real.",
            title="Fase 3 — hardware", border_style="yellow"))
        return

    run_dir = _pick_run(console, require_onnx=True)
    if run_dir is None:
        return
    fast = Confirm.ask("Modo rápido (subset dos datasets)?", default=True)
    import json

    from benchmark import run_offline_benchmark
    from logger import RunLogger
    from pipeline import _vitals_datasets, stage_validate
    from report import generate_html_report

    cfg = Config(fast_dev=fast)
    with RunLogger(run_dir) as log:
        availability = stage_validate(cfg, log)
        bench = run_offline_benchmark(
            cfg, run_dir, log,
            vitals_datasets=_vitals_datasets(cfg, availability))
        (run_dir / "benchmark.json").write_text(json.dumps(bench, indent=2))
        report = generate_html_report(run_dir, benchmark=bench)
    console.print(f"[bold green]Benchmark concluído.[/bold green] "
                  f"Relatório: {report}")


def action_report(console: Console) -> None:
    run_dir = _pick_run(console)
    if run_dir is None:
        return
    from report import generate_html_report

    report = generate_html_report(run_dir)
    console.print(f"[bold green]Relatório gerado:[/bold green] {report}")


# ---------------------------------------------------------------------------
# Loop principal
# ---------------------------------------------------------------------------

_MENU = """\
[bold cyan][1][/bold cyan] Smoke Test           (amostras pequenas, 1 época, relatório)
[bold cyan][2][/bold cyan] Pipeline Completo    (download integral + treino GPU)
[bold cyan][3][/bold cyan] Baixar datasets      (smoke ou completo)
[bold cyan][4][/bold cyan] Treinar modelo       (presence / occupancy / vitals)
[bold cyan][5][/bold cyan] Benchmark            (offline ou ao vivo)
[bold cyan][6][/bold cyan] Gerar relatório      (de uma run existente)
[bold cyan][0][/bold cyan] Sair"""

_ACTIONS = {
    "1": action_smoke,
    "2": action_full,
    "3": action_download,
    "4": action_train_one,
    "5": action_benchmark,
    "6": action_report,
}


def main() -> None:
    console = _console()
    console.print(Panel(
        "[bold]AetherSense ML[/bold] — detecção de presença, ocupação e "
        "vitais via WiFi CSI\nDatasets públicos apenas "
        "(restrição ética do TCC).",
        border_style="blue"))

    while True:
        console.print()
        console.print(Panel(_MENU, title="Menu", border_style="blue"))
        choice = Prompt.ask("Opção", choices=[*_ACTIONS, "0"], default="1")
        if choice == "0":
            console.print("Até mais!")
            return
        try:
            _ACTIONS[choice](console)
        except KeyboardInterrupt:
            console.print("\n[yellow]Interrompido — voltando ao menu.[/yellow]")
        except Exception as exc:
            console.print(f"[bold red]Erro: {exc}[/bold red]")
            import traceback

            traceback.print_exc()


if __name__ == "__main__":
    main()
