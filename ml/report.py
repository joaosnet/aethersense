"""
report.py — relatório HTML de uma run do pipeline AetherSense.

Gera um único arquivo auto-contido (run_dir/report.html): Jinja2 para o
template, matplotlib (backend Agg) para as figuras, embutidas como PNG
base64 — sem arquivos auxiliares, fácil de anexar ao TCC.

Conteúdo:
  - Configuração da run + resumo dos modelos treinados
  - Curvas de treino (train/val loss) extraídas do log.jsonl
  - Benchmark offline: tabelas por tarefa + confusion matrices,
    CDF do erro de localização e Bland-Altman dos vitais
"""

from __future__ import annotations

import base64
import io
import json
from datetime import datetime, timezone
from pathlib import Path
from typing import Optional

import matplotlib

matplotlib.use("Agg")

import matplotlib.pyplot as plt
import numpy as np
from jinja2 import Template


# ---------------------------------------------------------------------------
# Figuras → base64
# ---------------------------------------------------------------------------

def _fig_to_b64(fig) -> str:
    buf = io.BytesIO()
    fig.savefig(buf, format="png", dpi=110, bbox_inches="tight")
    plt.close(fig)
    return base64.b64encode(buf.getvalue()).decode("ascii")


def _training_curves(jsonl_path: Path) -> dict[str, str]:
    """Uma figura por modelo com as curvas train/val loss do log.jsonl."""
    if not jsonl_path.exists():
        return {}
    by_model: dict[str, list[dict]] = {}
    for line in jsonl_path.read_text(encoding="utf-8").splitlines():
        try:
            ev = json.loads(line)
        except json.JSONDecodeError:
            continue
        if ev.get("level") != "METRIC":
            continue
        model = ev.get("model", "occupancy_heatmap")
        by_model.setdefault(model, []).append(ev)

    figures: dict[str, str] = {}
    for model, events in by_model.items():
        events = sorted(events, key=lambda e: e.get("epoch", 0))
        epochs = [e.get("epoch", i + 1) for i, e in enumerate(events)]
        fig, ax = plt.subplots(figsize=(6, 3.2))
        for key, label in (("train_loss", "train"), ("val_loss", "val")):
            ys = [e.get(key) for e in events]
            if any(y is not None for y in ys):
                ax.plot(epochs, ys, marker="o", markersize=3, label=label)
        ax.set_xlabel("época")
        ax.set_ylabel("loss")
        ax.set_title(model)
        ax.legend()
        ax.grid(alpha=0.3)
        figures[model] = _fig_to_b64(fig)
    return figures


def _confusion_fig(matrix: list[list[int]], labels: list[str], title: str) -> str:
    cm = np.asarray(matrix)
    fig, ax = plt.subplots(figsize=(4.4, 3.8))
    im = ax.imshow(cm, cmap="Blues")
    ax.set_xticks(range(len(labels)), labels)
    ax.set_yticks(range(len(labels)), labels)
    ax.set_xlabel("predito")
    ax.set_ylabel("real")
    ax.set_title(title)
    thresh = cm.max() / 2 if cm.size and cm.max() else 0.5
    for i in range(cm.shape[0]):
        for j in range(cm.shape[1]):
            ax.text(j, i, str(cm[i, j]), ha="center", va="center",
                    color="white" if cm[i, j] > thresh else "black", fontsize=8)
    fig.colorbar(im, ax=ax, shrink=0.8)
    return _fig_to_b64(fig)


def _cdf_fig(distances_m: list[float], thresholds: tuple = (0.5, 1.0, 2.0)) -> Optional[str]:
    if not distances_m:
        return None
    d = np.sort(np.asarray(distances_m))
    cdf = np.arange(1, len(d) + 1) / len(d)
    fig, ax = plt.subplots(figsize=(6, 3.2))
    ax.plot(d, cdf, drawstyle="steps-post")
    for t in thresholds:
        frac = float(np.mean(d <= t))
        ax.axvline(t, color="gray", linestyle="--", alpha=0.5)
        ax.annotate(f"{frac:.0%} ≤ {t} m", (t, min(frac + 0.05, 0.97)), fontsize=8)
    ax.set_xlabel("erro de localização (m)")
    ax.set_ylabel("CDF")
    ax.set_ylim(0, 1.02)
    ax.set_title("CDF do erro de localização")
    ax.grid(alpha=0.3)
    return _fig_to_b64(fig)


def _bland_altman_fig(true_vals: list[float], pred_vals: list[float],
                      unit: str, title: str) -> Optional[str]:
    if len(true_vals) < 2:
        return None
    t = np.asarray(true_vals)
    p = np.asarray(pred_vals)
    mean = (t + p) / 2
    diff = p - t
    md, sd = float(diff.mean()), float(diff.std())
    fig, ax = plt.subplots(figsize=(6, 3.2))
    ax.scatter(mean, diff, s=14, alpha=0.6)
    ax.axhline(md, color="tab:red", label=f"viés {md:+.2f} {unit}")
    for k in (1.96, -1.96):
        ax.axhline(md + k * sd, color="gray", linestyle="--", alpha=0.7)
    ax.set_xlabel(f"média ({unit})")
    ax.set_ylabel(f"diferença pred − real ({unit})")
    ax.set_title(f"Bland-Altman — {title}")
    ax.legend(fontsize=8)
    ax.grid(alpha=0.3)
    return _fig_to_b64(fig)


# ---------------------------------------------------------------------------
# Template
# ---------------------------------------------------------------------------

_TEMPLATE = Template("""<!DOCTYPE html>
<html lang="pt-BR">
<head>
<meta charset="utf-8">
<title>AetherSense ML — {{ run_id }}</title>
<style>
  body { font-family: -apple-system, "Segoe UI", Roboto, sans-serif;
         max-width: 960px; margin: 2rem auto; padding: 0 1rem; color: #1a1a2e; }
  h1 { border-bottom: 3px solid #4361ee; padding-bottom: .4rem; }
  h2 { color: #3a0ca3; margin-top: 2.2rem; }
  table { border-collapse: collapse; margin: .8rem 0; }
  th, td { border: 1px solid #cbd5e1; padding: .35rem .7rem; text-align: left; }
  th { background: #eef2ff; }
  img { max-width: 100%; border: 1px solid #e2e8f0; border-radius: 6px;
        margin: .5rem 0; }
  .muted { color: #64748b; font-size: .9rem; }
  .skip { background: #fef9c3; padding: .5rem .8rem; border-radius: 6px; }
  code { background: #f1f5f9; padding: .1rem .35rem; border-radius: 4px; }
</style>
</head>
<body>
<h1>AetherSense ML — Relatório da Run</h1>
<p class="muted">Run <code>{{ run_id }}</code> · gerado em {{ generated_at }} ·
modo {{ "smoke test (fast-dev)" if fast_dev else "completo" }}</p>

<h2>Resumo do treino</h2>
<table>
<tr><th>Modelo</th><th>Status</th><th>Métricas finais</th></tr>
{% for name, info in training.items() %}
<tr>
  <td><code>{{ name }}</code></td>
  <td>{{ "pulado" if info.get("skipped") else "treinado" }}</td>
  <td>{% for k, v in info.items() if k not in ("skipped", "confusion_matrix")
        and v is not none %}{{ k }}={{ "%.4g"|format(v) if v is number else v }}{{ ", " if not loop.last }}{% endfor %}</td>
</tr>
{% endfor %}
</table>

{% if curves %}
<h2>Curvas de treino</h2>
{% for model, img in curves.items() %}
<img src="data:image/png;base64,{{ img }}" alt="curva {{ model }}">
{% endfor %}
{% endif %}

<h2>Benchmark offline (ONNX + ONNXRuntime)</h2>
{% if not benchmark %}
<p class="skip">Benchmark não executado nesta run.</p>
{% else %}

{% if benchmark.presence %}
<h3>Presença (PresenceCNN × Zenodo)</h3>
<table>
<tr><th>Amostras</th><th>Accuracy</th><th>Precision [vazio, ocupado]</th>
    <th>Recall</th><th>F1</th><th>Latência (ms)</th></tr>
<tr><td>{{ benchmark.presence.samples }}</td>
    <td>{{ "%.4f"|format(benchmark.presence.accuracy) }}</td>
    <td>{{ benchmark.presence.precision }}</td>
    <td>{{ benchmark.presence.recall }}</td>
    <td>{{ benchmark.presence.f1 }}</td>
    <td>{{ "%.2f"|format(benchmark.presence.latency_ms) }}</td></tr>
</table>
{% if figures.presence_cm %}<img src="data:image/png;base64,{{ figures.presence_cm }}">{% endif %}
{% endif %}

{% if benchmark.occupancy %}
<h3>Ocupação (OccupancyHeatmapNet × WiMANS)</h3>
<table>
<tr><th>Amostras</th><th>MAE contagem</th><th>Accuracy contagem</th>
    <th>Erro médio localização (m)*</th><th>Miss rate</th><th>Latência (ms)</th></tr>
<tr><td>{{ benchmark.occupancy.samples }}</td>
    <td>{{ "%.3f"|format(benchmark.occupancy.count_mae) }}</td>
    <td>{{ "%.4f"|format(benchmark.occupancy.count_accuracy) }}</td>
    <td>{{ "%.3f"|format(benchmark.occupancy.mean_distance_error_m)
           if benchmark.occupancy.mean_distance_error_m is not none else "—" }}</td>
    <td>{{ "%.1f%%"|format(benchmark.occupancy.location_miss_rate * 100)
           if benchmark.occupancy.location_miss_rate is not none else "—" }}</td>
    <td>{{ "%.2f"|format(benchmark.occupancy.latency_ms) }}</td></tr>
</table>
<p class="muted">* assumindo sala de
{{ benchmark.occupancy.room_size_m_assumed }} m de lado (anchors WiMANS são
normalizados; dimensão real não publicada).</p>
<table>
<tr><th>CDF</th>{% for k in benchmark.occupancy.location_cdf %}<th>≤ {{ k }}</th>{% endfor %}</tr>
<tr><td>fração</td>{% for v in benchmark.occupancy.location_cdf.values() %}
  <td>{{ "%.1f%%"|format(v * 100) if v is not none else "—" }}</td>{% endfor %}</tr>
</table>
{% if figures.occupancy_cm %}<img src="data:image/png;base64,{{ figures.occupancy_cm }}">{% endif %}
{% if figures.location_cdf %}<img src="data:image/png;base64,{{ figures.location_cdf }}">{% endif %}
{% endif %}

{% if benchmark.vitals %}
<h3>Vitais (VitalsNet × CSI-MiningTool / NIST)</h3>
<table>
<tr><th>Amostras</th><th>MAE respiração (RPM)</th><th>Pearson resp.</th>
    <th>MAE coração (BPM)</th><th>Pearson coração</th><th>Latência (ms)</th></tr>
<tr><td>{{ benchmark.vitals.samples }}</td>
    <td>{{ "%.2f"|format(benchmark.vitals.mae_breathing_rpm)
           if benchmark.vitals.mae_breathing_rpm is not none else "—" }}</td>
    <td>{{ "%.3f"|format(benchmark.vitals.pearson_breathing)
           if benchmark.vitals.pearson_breathing is not none else "—" }}</td>
    <td>{{ "%.2f"|format(benchmark.vitals.mae_heart_bpm)
           if benchmark.vitals.mae_heart_bpm is not none else "—" }}</td>
    <td>{{ "%.3f"|format(benchmark.vitals.pearson_heart)
           if benchmark.vitals.pearson_heart is not none else "—" }}</td>
    <td>{{ "%.2f"|format(benchmark.vitals.latency_ms) }}</td></tr>
</table>
{% if figures.bland_altman_breathing %}<img src="data:image/png;base64,{{ figures.bland_altman_breathing }}">{% endif %}
{% if figures.bland_altman_heart %}<img src="data:image/png;base64,{{ figures.bland_altman_heart }}">{% endif %}
{% endif %}

{% for name in ("presence", "occupancy", "vitals") %}
{% if benchmark[name] is none %}
<p class="skip">Benchmark de <code>{{ name }}</code> não executado
(modelo ou dataset indisponível).</p>
{% endif %}
{% endfor %}
{% endif %}

<h2>Restrição ética</h2>
<p class="muted">Pipeline treinado exclusivamente com datasets públicos
(Zenodo 8021099, WiMANS, NIST BreatheSmart, WiFi-CSI-MiningTool, Wi-Pose,
MM-Fi). Nenhuma coleta local de dados humanos. MM-Fi sob licença
CC BY-NC 4.0 (uso acadêmico, citação obrigatória).</p>
</body>
</html>
""")


# ---------------------------------------------------------------------------
# Entrada principal
# ---------------------------------------------------------------------------

def generate_html_report(
    run_dir: Path,
    training: Optional[dict] = None,
    benchmark: Optional[dict] = None,
) -> Path:
    """Gera run_dir/report.html. *training*/*benchmark* são opcionais —
    quando ausentes, tenta reconstruir do que existe em run_dir."""
    run_dir = Path(run_dir)
    if training is None:
        summary_path = run_dir / "pipeline_summary.json"
        training = {}
        if summary_path.exists():
            training = json.loads(summary_path.read_text(encoding="utf-8")).get(
                "training", {})
    if benchmark is None:
        bench_path = run_dir / "benchmark.json"
        benchmark = (json.loads(bench_path.read_text(encoding="utf-8"))
                     if bench_path.exists() else None)

    config = {}
    cfg_path = run_dir / "config.json"
    if cfg_path.exists():
        config = json.loads(cfg_path.read_text(encoding="utf-8"))

    figures: dict[str, Optional[str]] = {}
    if benchmark:
        if benchmark.get("presence"):
            figures["presence_cm"] = _confusion_fig(
                benchmark["presence"]["confusion_matrix"],
                ["vazio", "ocupado"], "Presença")
        if benchmark.get("occupancy"):
            occ = benchmark["occupancy"]
            n = len(occ["confusion_matrix"])
            figures["occupancy_cm"] = _confusion_fig(
                occ["confusion_matrix"],
                [str(i) for i in range(n - 1)] + [f"{n - 1}+"], "Contagem")
            figures["location_cdf"] = _cdf_fig(occ.get("location_distances_m", []))
        if benchmark.get("vitals"):
            v = benchmark["vitals"]
            figures["bland_altman_breathing"] = _bland_altman_fig(
                v.get("breathing_true_rpm", []), v.get("breathing_pred_rpm", []),
                "RPM", "respiração")
            figures["bland_altman_heart"] = _bland_altman_fig(
                v.get("heart_true_bpm", []), v.get("heart_pred_bpm", []),
                "BPM", "batimento")

    html = _TEMPLATE.render(
        run_id=run_dir.name,
        generated_at=datetime.now(timezone.utc).strftime("%Y-%m-%d %H:%M UTC"),
        fast_dev=bool(config.get("fast_dev")),
        training=training or {},
        curves=_training_curves(run_dir / "log.jsonl"),
        benchmark=benchmark,
        figures=figures,
    )
    out = run_dir / "report.html"
    out.write_text(html, encoding="utf-8")
    return out
