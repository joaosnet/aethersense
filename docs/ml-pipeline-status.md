# AetherSense ML Pipeline — Status & Architecture Reference

> **Última atualização**: 2026-06-10
> **Propósito**: Documento de referência para agentes AI e desenvolvedores sobre o estado atual do pipeline de ML.

## Visão Geral

O AetherSense usa WiFi CSI (Channel State Information) para detectar presença, contar e localizar pessoas, e estimar sinais vitais (batimento cardíaco e respiração) em ambientes internos. O pipeline de ML treina modelos offline com datasets públicos e exporta ONNX para inferência no servidor Rust.

### Fluxo de Dados

```
ESP32-C5/WROOM (firmware-rust/)
  → CSI_DATA CSV via UDP:5005
  → aethersense-server/ (Rust: parse → heurística atual)
  → REST/WS → frontend/ (React dashboard)
  → MQTT → Home Assistant

ml/ (Python, standalone)
  → Treina com datasets públicos
  → Exporta ONNX
  → Futuro: servidor carrega ONNX via tract-onnx
```

---

## Estado Atual (2026-06-10)

### O que já existe e funciona ✅

| Componente | Arquivo | Status |
|---|---|---|
| Download de datasets | `ml/download_datasets.py` | ✅ Zenodo + WiMANS + Wi-Pose (download) |
| Dataset loaders | `ml/datasets.py` | ✅ `ZenodoPresenceDataset` + `WiMANSDataset` |
| Pré-processamento | `ml/preprocessing.py` | ✅ CSI→matrix, Z-score, heatmap, NMS |
| Modelo de presença | `ml/models.py` → `PresenceCNN` | ✅ Binary CNN (Zenodo) |
| Modelo de ocupação | `ml/models.py` → `OccupancyHeatmapNet` | ✅ U-Net ~482k params (WiMANS) |
| Treino end-to-end | `ml/train.py` | ✅ Train → checkpoint → ONNX export → ORT validation |
| Export ONNX | `ml/export_onnx.py` | ✅ Opset 17, validado |
| Checkpointing | `ml/checkpointing.py` | ✅ Best-val-loss + periodic, resume support |
| Logger | `ml/logger.py` | ✅ Rich console + JSONL + text file |
| Config | `ml/config.py` | ✅ Dataclass com CLI |
| Smoke test | `main.py` opção [1] (ou `pipeline.py --fast-dev`) | ✅ Passou em 2026-06-10 |
| TUI interativo | `ml/main.py` | ✅ Menu Rich (smoke, completo, download, treino, benchmark, relatório) |
| Pipeline orquestrador | `ml/pipeline.py` | ✅ Download → validate → train (3 modelos) → benchmark → report |
| 4 novos datasets | `ml/datasets.py` + `ml/download_datasets.py` | ✅ Loaders + download (MM-Fi CSI e Wi-Pose são opt-in; ver notas) |
| Modelo de vitais | `ml/models.py` → `VitalsNet` | ✅ 1D CNN + LSTM, 190k params, saída em Hz com faixas fisiológicas |
| Trainers satélites | `ml/trainers.py` | ✅ `train_presence` + `train_vitals` (L1 mascarada p/ GT parcial) |
| Framework de benchmark | `ml/benchmark.py` | ✅ Offline sobre os ONNX exportados; ao vivo = stub (Fase 3) |
| Relatório HTML | `ml/report.py` | ✅ Jinja2 + matplotlib, auto-contido (PNGs base64) |
| Setup Thunder Computer | `setup_thunder.sh` (raiz do repo) | ✅ uv + deps + torch CUDA + sanity check |

### Integração online (2026-06-12) ✅

| Componente | Arquivo | Status |
|---|---|---|
| Inferência ONNX no servidor Rust | `aethersense-server/src/ml/` (tract-onnx) | ✅ occupancy (CNN) + vitals (LSTM) carregam e rodam |
| Extração de features live | `src/ml/features.rs` | ✅ réplica do `finalize_window` (64 subc, z-score, janela tail) |
| Pessoas multi-alvo georreferenciadas | `src/ml/decode.rs` | ✅ picos do heatmap + NMS 5×5 → lat/lng via floorplan/receptores |
| API ML (`GET /api/ml/status`) | `src/api/handlers.rs` | ✅ runtime + comparação ML × heurística |
| `occupancy` no DetectionState (REST/WS) | `src/state.rs` | ✅ contagem, probabilidades, pessoas, vitais ML |
| Frontend com ocupação ML | `Header`, `AetherMap`, `SystemPipeline` | ✅ contagem 0..5+, marcadores P1..P5 com vitais, probabilidades |
| MQTT discovery ML | `src/mqtt.rs` | ✅ +5 sensores (count, conf, method, resp ML, bat ML) |
| Treino completo em GPU | run `20260612_121755_0c499459` (A6000) | ✅ concluído |

**Vitais por pessoa — limitação honesta:** os datasets públicos não têm ground
truth de vitais multi-pessoa; com 1 pessoa o campo `vitals_scope` é
`individual`, com 2+ a taxa publicada é a global do ambiente (`global`).
A FFT heurística continua publicada em `detection.vitals` para comparação.

### Comparação ML × heurística

| Métrica | Heurística (hardware real, 2026-05-11) | ML ONNX (validação WiMANS, 2026-06-12) |
|---|---|---|
| Localização (erro) | `accuracy_m = 0.6848` | `0.435 m` médio, `91.2% ≤ 0.5 m` |
| Multi-pessoa | 1 alvo (centroide fusionado) | até 5 pessoas simultâneas |
| Contagem | não suportada (binário presença) | exata `51.7%`, ±1 pessoa `78.6%` |
| Vitais | FFT global experimental | VitalsNet global (resp MAE 2.35 RPM, card 4.97 BPM no val) |
| Latência | <1 ms | ~2-5 ms/frame (tract CPU) |

*Caveats: o número ML vem de validação offline em dataset público (ambiente
diferente); o da heurística vem de medição live no ambiente real. A comparação
ao vivo lado a lado roda no servidor (`/api/ml/status`) — a validação ML em
campo com ESP32 é o próximo experimento (requer hardware ligado).*

### O que ainda não existe ❌

| Componente | Bloqueio | Referência |
|---|---|---|
| Validação ao vivo ML × heurística com ESP32 | Requer hardware ligado | `/api/ml/status` já expõe os dois lados |
| Calibração de domínio (sala vazia → ML) | Trabalho futuro | restrição ética permite só sala vazia |

---

## Datasets

### Restrição Ética Crítica

> **PROIBIDO** coletar datasets próprios que envolvam presença ou locomoção de seres humanos.
> Usar **exclusivamente** datasets públicos. A única adaptação local permitida é **calibração de sala vazia** (Background Subtraction).

### Datasets Integrados

| # | Dataset | Fonte | Formato | Uso | Licença |
|---|---|---|---|---|---|
| 1 | **Zenodo 8021099** | [Zenodo API](https://zenodo.org/records/8021099) | PNG 64×64 + CSV labels | Presença binária | Public |
| 2 | **WiMANS** | [Kaggle](https://www.kaggle.com/datasets/shuokanghuang/wimans) | `.npy` ~`(T, 3, 3, 30)` float32 | Contagem (0-5) + localização | Public |
| 3 | **Wi-Pose** | [Google Drive](https://drive.google.com/file/d/1WL6bJ-rSVdsclRt9RFc0l5hhtXYmfNg9) | `.mat` → `5×30×3×3` | Pose (auxiliar de ocupação) | Research use |
| 4 | **MM-Fi** | [GitHub toolbox](https://github.com/ybhbingo/MMFi_dataset) | `.mat` chave `CSIamp` → `3×114×10` | Pose + atividade (auxiliar de ocupação) | **CC BY-NC 4.0** |
| 5 | **WiFi-CSI-MiningTool** | [GitHub](https://github.com/AlbanyArmenta0711/WiFi-CSI-MiningTool) | CSV bruto 181 col (ts + 90 amp + 90 fase, ~25 Hz) | BR (9–21 RPM, nome do arquivo) + HR (`HRReal.CSV`) | **GPL-3.0** |
| 6 | **NIST BreatheSmart** | [NIST.gov](https://data.nist.gov/od/id/mds2-2963) | zip → tar.gz → CSVs real/imag `N×1026` | Breathing rate (manequim, campo `bpm`; **sem HR**) | US Gov public domain |

> **Nota sobre MM-Fi**: Licença CC BY-NC 4.0 (não-comercial). Aceitável para TCC acadêmico, mas deve ser citada com a licença explícita. O CSI WiFi **não é separável** das outras modalidades: o menor download é um ambiente (`E0x.zip`, ~18–21 GB) — por isso é opt-in (`--include-mmfi`); o smoke baixa apenas as anotações (`MMFi_action_segments.csv`).
> **Nota sobre WiFi-CSI-MiningTool**: o repositório também traz matrizes prontas com 161 features estatísticas + label (classificação discreta), mas o `VitalsNet` consome o **CSV bruto** (janelas de amplitude CSI). Licença GPL-3.0 (copyleft) — citar o paper (Armenta-Garcia et al., PMC 2023, DOI 10.1016/j.pmcj.2023.101768).
> **Nota sobre NIST**: ground truth só de respiração — o canal de batimento fica `NaN` e o treino usa L1 mascarada. No smoke, membros individuais (~4 MB) são extraídos do zip remoto de 202 MB via HTTP Range.

### Formato CSI de Cada Dataset

| Dataset | Shape Original | Reshape para Modelo | Subcarriers | Timesteps |
|---|---|---|---|---|
| Zenodo | `(64, 64)` PNG | `(1, 64, 64)` | 64 | 64 |
| WiMANS | `(T≈2835–2936, 3, 3, 30)` | `(1, 64, 50)` | 30→64 (interp) | T→50 (slice) |
| Wi-Pose | `(5, 30, 3, 3)` | `(1, 64, 50)` | 30→64 (interp) | 5→50 (pad) |
| MM-Fi | `(3, 114, 10)` × frames | `(1, 64, 50)` | 114→64 (interp) | 10·frames→50 |
| CSI-MiningTool | CSV `(N, 181)` → 90 amp | `(1, 64, 500)` | 90→64 (interp) | janelas de 500 |
| NIST | CSVs real/imag `(N, 1026)` | `(1, 64, 500)` | não-zero→64 (interp) | janela de 500 |

---

## Modelos

### 3 Modelos Especializados (separados)

| # | Modelo | Classe | Input | Output | Datasets de Treino |
|---|---|---|---|---|---|
| 1 | **Presença** | `PresenceCNN` | `(B, 1, 64, 64)` | `(B, 2)` logits [vazio, ocupado] | Zenodo 8021099 |
| 2 | **Ocupação + Localização** | `OccupancyHeatmapNet` | `(B, 1, 64, 50)` | `(B, 6)` count + `(B, 1, 20, 20)` heatmap | WiMANS + Wi-Pose + MM-Fi |
| 3 | **Vitais** | `VitalsNet` (novo) | `(B, 1, 64, T)` | `(B, 2)` [breathing_hz, heart_hz] | CSI-MiningTool + NIST |

### Abordagem de Vitais: Híbrida

- **FFT heurístico** (já implementado no servidor) como baseline confiável
- **VitalsNet** como refinamento ML, treinado em datasets públicos
- Se datasets de vitais forem insuficientes → FFT continua como método primário
- Fallback automático: se VitalsNet tem baixa confiança → usar FFT

### ONNX Export

Todos os modelos são exportados como ONNX (opset 17) com metadata JSON:
- Input/output shapes
- Dataset de origem
- Métricas de validação
- Configuração de normalização

---

## Como Rodar

### Smoke Test (local, sem GPU)

```bash
cd ml/
uv run main.py
# Escolher opção [1] Smoke Test
# ou, sem TUI:  uv run pipeline.py --fast-dev
```

Baixa amostras pequenas de Zenodo, WiMANS, CSI-MiningTool e NIST (+ anotações
do MM-Fi; Wi-Pose e CSI do MM-Fi pulados — só existem como zips grandes),
treina 1 epoch dos 3 modelos, roda o benchmark offline e gera o relatório.

### Treino Completo (Thunder Computer, com GPU)

```bash
# No Thunder Computer:
bash setup_thunder.sh          # Verifica pré-requisitos, instala uv
cd ml/
uv run main.py
# Escolher opção [2] Pipeline Completo
```

### Benchmark

```bash
cd ml/
uv run main.py
# Escolher opção [5] Benchmark
# → [1] Offline (datasets) ou [2] Ao vivo (ESP32)
```

---

## Métricas de Validação

| Tarefa | Métricas |
|---|---|
| Presença | Accuracy, Precision, Recall, F1-Score, Confusion Matrix |
| Contagem | MAE, Accuracy por classe (0-5+), Confusion Matrix |
| Localização | Mean Distance Error (m), CDF de erro (0.5m, 1m, 2m) |
| Batimento Cardíaco | MAE (BPM), Correlação Pearson, Bland-Altman plot |
| Respiração | MAE (RPM), Correlação Pearson |
| Latência | Tempo médio inferência/frame (ms) |
| ML vs Heurística | Comparação side-by-side para todas as métricas |

### Configurações de Hardware (apenas modo ao vivo, Fase 3)

- 1× ESP32-C5
- N× ESP32-C5
- 1× ESP32-WROOM
- N× ESP32-WROOM
- Misto (C5 + WROOM)

No modo offline, um único modelo genérico é treinado. A diferenciação por hardware ocorre apenas com dados ao vivo.

---

## Fases de Implementação

### Fase 1 — Local (concluída em 2026-06-10)

- [x] Smoke test básico (Zenodo + WiMANS)
- [x] TUI interativo (`main.py`)
- [x] Pipeline orquestrador (`pipeline.py`)
- [x] Download de MM-Fi, CSI-MiningTool, NIST BreatheSmart
- [x] Dataset classes para os 4 novos datasets
- [x] `VitalsNet` modelo de vitais
- [x] Treino multi-modelo
- [x] Framework de benchmark offline
- [x] Relatório HTML automático
- [x] Smoke test com todos os datasets baixáveis (Zenodo, WiMANS,
      CSI-MiningTool, NIST; MM-Fi só anotações e Wi-Pose opt-in — os CSI
      deles não existem em amostras pequenas)

### Fase 2 — Thunder Computer

- [x] `setup_thunder.sh`
- [ ] Treino completo com GPU (40+ epochs)
- [ ] Validação de métricas com datasets completos
- [ ] Export dos 3 modelos ONNX finais

### Fase 3 — Hardware Real

- [ ] Benchmark ao vivo com ESP32
- [ ] Validação por configuração de hardware
- [ ] Comparação ML vs heurística com dados reais
- [ ] Relatório final para o TCC

---

## Arquivos de Referência

| Arquivo | Descrição |
|---|---|
| `ml/README.md` | README do pipeline ML |
| `ml/main.py` | TUI interativo (entrada principal) |
| `ml/pipeline.py` | Orquestrador download→validate→train→benchmark→report |
| `ml/config.py` | Configuração centralizada |
| `ml/models.py` | PresenceCNN, OccupancyHeatmapNet, VitalsNet |
| `ml/train.py` | Loop de treino do OccupancyHeatmapNet |
| `ml/trainers.py` | Loops de treino do PresenceCNN e VitalsNet |
| `ml/benchmark.py` | Benchmark offline dos ONNX exportados |
| `ml/report.py` | Relatório HTML (Jinja2 + matplotlib) |
| `ml/download_datasets.py` | Download dos 6 datasets |
| `ml/datasets.py` | Classes Dataset PyTorch (6 datasets) |
| `ml/preprocessing.py` | Pré-processamento CSI |
| `ml/THUNDER_COMPUTE.md` | Guia Thunder Computer |
| `setup_thunder.sh` | Setup automatizado da instância (raiz do repo) |
| `docs/ml-next-steps-after-validation.md` | Plano pós-validação (servidor Rust) |
| `docs/arquivo/ml-colab-domain-adaptation.md` | Design da fase Colab (histórico, arquivado) |
| `firmware-rust/src/csi.rs` | Formato CSI do firmware |
| `aethersense-server/src/csi/parser.rs` | Parser CSI do servidor |
| `aethersense-server/src/detection/` | Algoritmos heurísticos atuais |
