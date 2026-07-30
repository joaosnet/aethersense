# AetherSense ML Pipeline

Scripts Python para treinamento do modelo de detecção de ocupação via CSI.
Substitui o notebook `notebooks/aethersense_colab_ml_pipeline.ipynb`.

## Estrutura

```
ml/
├── pyproject.toml      # Dependências gerenciadas pelo uv
├── main.py             # TUI interativa (menu Rich) — entrada principal
├── pipeline.py         # Orquestrador: download → validate → train → benchmark → report
├── config.py           # Config dataclass + argparse CLI
├── logger.py           # Dual logger: console Rich + JSON Lines + texto
├── checkpointing.py    # Save/load checkpoints + CheckpointManager
├── preprocessing.py    # Funções de pré-processamento CSI
├── download_datasets.py # Download dos 6 datasets públicos (sem credenciais)
├── datasets.py         # 6 Dataset classes (Zenodo, WiMANS, Wi-Pose, MM-Fi,
│                       #   CSI-MiningTool, NIST BreatheSmart)
├── models.py           # PresenceCNN, OccupancyHeatmapNet, VitalsNet
├── train.py            # Loop de treino do OccupancyHeatmapNet
├── trainers.py         # Loops de treino do PresenceCNN e VitalsNet
├── benchmark.py        # Benchmark offline dos ONNX exportados
├── report.py           # Relatório HTML (Jinja2 + matplotlib)
├── export_onnx.py      # Exportação ONNX standalone
└── artifacts/          # Gerado automaticamente; no .gitignore
    └── runs/
        └── <run_id>/
            ├── log.jsonl           # Eventos JSON para documentação TCC
            ├── log.txt             # Transcrição legível para o TCC
            ├── config.json         # Config da run (reprodutibilidade)
            ├── checkpoint_presence_cnn_best.pt
            ├── checkpoint_occupancy_heatmap_best.pt
            ├── checkpoint_occupancy_heatmap_epoch0050.pt
            ├── occupancy.onnx
            ├── occupancy_metadata.json
            └── metrics_summary.json
```

## Instalação

```bash
cd ml/

# Instalar dependências com uv
uv sync

# Com suporte CUDA (Thunder Compute / qualquer servidor Linux com GPU)
pip install torch --index-url https://download.pytorch.org/whl/cu124
uv sync
```

## Uso

### TUI interativa (recomendado)
```bash
uv run main.py
# [1] Smoke Test  [2] Pipeline Completo  [3] Download  [4] Treino
# [5] Benchmark   [6] Relatório
```

### Pipeline completo sem TUI (3 modelos + benchmark + relatório)
```bash
uv run pipeline.py --fast-dev      # smoke test
uv run pipeline.py                 # completo (GPU recomendada)
```

### Baixar os datasets (sem credenciais)
```bash
# Completo: WiMANS ~19 GB + CSI-MiningTool ~459 MB + NIST ~202 MB + Zenodo
uv run download_datasets.py

# Amostra pequena para smoke test (~70 MB no total)
uv run download_datasets.py --smoke
```

### Treinar só o OccupancyHeatmapNet (fluxo original)
```bash
uv run download_datasets.py --smoke
uv run train.py --fast-dev         # ou uv run train.py para completo
```

### Controlar número de épocas ou diretório de dados
```bash
uv run train.py --epochs 80 --data-dir /data/aethersense/datasets
```

### O que o script faz, em ordem:
1. Lê o dataset WiMANS (`datasets/WiMANS/` — baixado por `download_datasets.py`)
2. Treina `OccupancyHeatmapNet` (482k parâmetros)
3. Salva checkpoints (`best` + a cada 50 épocas)
4. Exporta automaticamente `occupancy.onnx` (opset 17)
5. Valida com ONNXRuntime
6. Salva `metrics_summary.json` + `log.jsonl` + `log.txt`

---

## Executar em segundo plano (Thunder Compute / Linux)

```bash
# Em background com nohup
nohup uv run train.py > nohup.out 2>&1 &

# Ver progresso em tempo real
tail -f nohup.out

# Ver log estruturado da run
tail -f ml/artifacts/runs/<run_id>/log.txt

# Ver métricas por época (JSON)
cat ml/artifacts/runs/<run_id>/log.jsonl | grep '"METRIC"'
```

---

## Retomar treinamento de um checkpoint

```bash
# Retomar do melhor checkpoint
uv run train.py \
  --resume ml/artifacts/runs/<run_id>/checkpoint_occupancy_heatmap_best.pt

# Retomar em outra máquina
scp user@thunder:/path/checkpoint_best.pt ./local_checkpoint.pt
uv run train.py --resume ./local_checkpoint.pt
```

---

## Exportar ONNX de um checkpoint existente

```bash
uv run export_onnx.py \
  --checkpoint ml/artifacts/runs/<run_id>/checkpoint_occupancy_heatmap_best.pt

# Saída vai para o mesmo diretório do checkpoint por padrão
# Ou especificar diretório diferente:
uv run export_onnx.py \
  --checkpoint <path> \
  --output-dir ml/artifacts/export/
```

---

## Logs para o TCC

Cada run gera três arquivos de log:

| Arquivo | Formato | Uso |
|---|---|---|
| `log.jsonl` | JSON Lines | Análise programática, gráficos de treinamento |
| `log.txt` | Texto puro | Anexar ao relatório do TCC |
| `metrics_summary.json` | JSON | Métricas finais consolidadas |

### Exemplo de análise dos logs JSON:

```python
import json
from pathlib import Path

# Carregar histórico de métricas de uma run
run_dir = Path("ml/artifacts/runs/<run_id>")
events = [json.loads(line) for line in (run_dir / "log.jsonl").read_text().splitlines()]

# Filtrar apenas eventos de métrica
metrics = [e for e in events if e["level"] == "METRIC"]

# Extrair curva de loss
losses = [(e["epoch"], e["val_loss"]) for e in metrics if "val_loss" in e]
```

---

## Estratégia de checkpointing

| Evento | Arquivo salvo |
|---|---|
| Val loss melhora | `checkpoint_occupancy_heatmap_best.pt` (sobrescreve) |
| Múltiplos de 50 épocas | `checkpoint_occupancy_heatmap_epoch0050.pt` (acumula) |

Cada checkpoint contém:
- `model_state` — pesos do modelo
- `optimizer_state` — estado do AdamW (momentum, lr, etc.)
- `config` — todos os hiperparâmetros da run
- `metrics_history` — histórico de métricas por época
- `epoch` — época atual
- `run_id` — identificador único da run

---

## Restrição ética (TCC)

Este pipeline utiliza **exclusivamente datasets públicos** (Zenodo 8021099, WiMANS).
Nenhuma coleta de dados humanos locais é feita. Esta restrição é registrada
em todos os metadados ONNX exportados.
