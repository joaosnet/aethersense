# Treinamento no Thunder Compute (GPU)

Guia para rodar o pipeline ML do AetherSense (`OccupancyHeatmapNet`) em uma
instância GPU do [Thunder Compute](https://www.thundercompute.com/) — ou em
qualquer servidor Linux com CUDA. O treino local em Windows funciona em CPU,
mas o dataset WiMANS completo (~11k amostras) só é viável em GPU.

> **Restrição ética (TCC):** use **exclusivamente** datasets públicos
> (ex.: WiMANS). É proibido coletar dados humanos locais. Veja `AGENTS.md`.

---

## 0. Setup automatizado (recomendado)

O `setup_thunder.sh` na raiz do repositório faz os passos 1–3 de uma vez
(verifica pré-requisitos, instala uv, sincroniza deps, instala torch CUDA e
roda o sanity check):

```bash
git clone <URL_DO_REPO> aethersense && cd aethersense
bash setup_thunder.sh
cd ml/
uv run main.py        # TUI: [1] Smoke Test → [2] Pipeline Completo
```

Os passos abaixo são o equivalente manual.

## 1. Pré-requisitos

- Instância Linux com GPU NVIDIA + driver CUDA (ex.: T4, A10, L4).
- `git`, `curl` e [`uv`](https://docs.astral.sh/uv/) instalados.

```bash
# Instalar uv (gerenciador de pacotes/ambiente Python do projeto)
curl -LsSf https://astral.sh/uv/install.sh | sh
```

## 2. Clonar o repositório

```bash
git clone <URL_DO_REPO> aethersense
cd aethersense/ml
```

## 3. Instalar dependências

O `pyproject.toml` + `uv.lock` (versionado) garantem ambiente reprodutível.

```bash
# Resolve e instala todas as dependências travadas no uv.lock
uv sync

# PyTorch com CUDA (ajuste cu124 conforme a versão de CUDA da instância)
uv pip install torch --index-url https://download.pytorch.org/whl/cu124

# Sanity check: deve imprimir True
uv run python -c "import torch; print(torch.cuda.is_available())"
```

## 4. Baixar os datasets

O script `download_datasets.py` baixa tudo automaticamente — **nenhuma
credencial é necessária** (Zenodo via API pública; WiMANS via endpoint
anônimo do Kaggle, arquivo a arquivo):

```bash
# Smoke test: amostra pequena (1 zona do Zenodo + ~24 .npy do WiMANS
# estratificados pelas classes de contagem 0..5; ~40 MB)
uv run download_datasets.py --smoke

# Treino completo: Zenodo inteiro (~14 MB) + WiMANS wifi_csi/amp inteiro
# (11.286 .npy, ~19 GB transferidos; vídeo/.mat do WiMANS não são baixados)
uv run download_datasets.py
```

O download completo é retomável: rodar de novo pula os arquivos já baixados.
Ao final, o script valida os dados com as próprias classes de `datasets.py`.
Estrutura produzida sob `datasets/` (raiz do repositório, um nível acima de `ml/`):

```
datasets/
├── zenodo_8021099/
│   └── DA_LOS/ …           # PNGs 64×64 + trainLabels.csv etc. (4 zonas no completo)
└── WiMANS/
    ├── annotation.csv      # no --smoke, contém SÓ as amostras baixadas
    └── wifi_csi/
        └── amp/
            ├── act_1_1.npy # amplitude CSI (T≈2835–2936, 3, 3, 30) float32
            └── …
```

> O diretório `datasets/` é git-ignored — os dados nunca entram no repositório.
> Importante: depois de um `--smoke`, rode `uv run download_datasets.py` (sem
> flag) antes do treino completo — o modo smoke grava um `annotation.csv`
> parcial, e o modo completo o substitui pelo integral.

## 5. Smoke test (1 época, subconjunto)

Valide o pipeline ponta a ponta antes do treino longo:

```bash
uv run download_datasets.py --smoke
uv run train.py --fast-dev
```

Isso roda 1 época em até 256 amostras e exporta um ONNX de teste. Se concluir
sem erro, o ambiente está pronto.

## 6. Treino completo (em background)

Pipeline completo — 3 modelos (PresenceCNN, OccupancyHeatmapNet, VitalsNet)
+ benchmark + relatório HTML:

```bash
nohup uv run pipeline.py > pipeline.out 2>&1 &
tail -f pipeline.out
```

Ou somente o OccupancyHeatmapNet (fluxo original):

```bash
# Garante o dataset completo (retomável; pula o que já existe)
uv run download_datasets.py

# Roda destacado do terminal; sobrevive a logout/SSH drop
nohup uv run train.py --epochs 40 > nohup.out 2>&1 &

# Acompanhar o progresso
tail -f nohup.out
# ou o log estruturado do run:
tail -f artifacts/runs/<run_id>/log.txt
```

Flags úteis (`config.py:from_args`):

| Flag | Efeito |
|------|--------|
| `--epochs N` | Número de épocas (default 40) |
| `--batch-size N` | Tamanho do batch (default 64) |
| `--lr F` | Learning rate (default 1e-3) |
| `--data-dir PATH` | Raiz alternativa dos datasets |
| `--resume PATH` | Retomar de um checkpoint `.pt` |
| `--fast-dev` | Smoke test (1 época, subset) |

## 7. Detecção de múltiplas pessoas (até 5)

O modelo escala o número de pessoas via `cfg.max_count` (default **5**, o
limite do WiMANS). A cabeça de contagem produz `max_count + 1` classes
(`0, 1, …, 5`, onde a última significa "5 ou mais"). Mudanças relevantes:

- **`config.py`** — `max_count = 5`, `use_class_weights = True`
  (compensa o desbalanceamento: muitas amostras de 1 pessoa, poucas de 0/5).
- **`models.py`** — `OccupancyHeatmapNet(max_count=…)` dimensiona a saída.
- **Métricas por classe** — `metrics_summary.json` traz
  `per_class_count_metrics` (precision/recall/F1 por número de pessoas),
  essencial para diagnosticar classes raras que a acurácia global esconde.

Para experimentar outro limite, não há flag dedicada ainda: ajuste
`max_count` em `config.py` (e retreine).

## 8. Artefatos gerados

Em `ml/artifacts/runs/<run_id>/`:

| Arquivo | Conteúdo |
|---------|----------|
| `occupancy.onnx` | Modelo exportado (opset 17), consumido pelo backend |
| `occupancy_metadata.json` | Contrato I/O: shape, classes, `max_count`, anchors |
| `metrics_summary.json` | Loss, matriz de confusão, métricas por classe |
| `checkpoint_occupancy_heatmap_best.pt` | Melhor checkpoint (menor val_loss) |
| `config.json` | Config exata do run (reprodutibilidade) |
| `log.txt` / `log.jsonl` | Histórico de treino |

## 9. Baixar resultados de volta

```bash
# Da sua máquina local
scp -r <user>@<thunder-host>:~/aethersense/ml/artifacts/runs/<run_id> ./
```

O `occupancy.onnx` + `occupancy_metadata.json` são o que o servidor Rust
precisa para inferência. Veja `docs/ml-next-steps-after-validation.md`.

---

## Solução de problemas

- **`torch.cuda.is_available()` é `False`** — reinstale o torch com o índice
  CUDA correto (passo 3) e confira `nvidia-smi`.
- **`WiMANS annotation.csv not found`** — rode `uv run download_datasets.py`
  (ou `--smoke`) e confira o caminho `datasets/WiMANS/`.
- **Download do WiMANS lento ou com falhas 429** — reduza a concorrência com
  `uv run download_datasets.py --workers 2` e rode de novo (retomável).
- **`num_workers`** é auto-detectado (0 no Windows, 4 no Linux) em
  `config.py:__post_init__` — não precisa configurar.
- **OOM na GPU** — reduza `--batch-size` (ex.: `--batch-size 32`).
