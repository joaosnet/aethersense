# Proximas Implementacoes Depois da Validacao do ONNX

> **STATUS 2026-06-12: IMPLEMENTADO.** O gate foi satisfeito (treino completo
> na A6000, run `20260612_121755_0c499459`) e a fase online descrita abaixo
> foi implementada: `aethersense-server/src/ml/` (tract-onnx), campo
> `occupancy` no `DetectionState`, `GET /api/ml/status`, frontend multi-pessoa
> e MQTT discovery ML. Estado atual em `docs/ml-pipeline-status.md`. Este
> documento permanece como referência do plano original.

Este documento guarda o plano da fase online, mas ela nao deve ser implementada antes de o pipeline `ml/` (scripts Python rodados em GPU no Thunder Compute, veja `ml/THUNDER_COMPUTE.md`) produzir um modelo ONNX treinado, testado e validado.

> O pipeline exporta **3 modelos**: `presence.onnx`, `occupancy.onnx` e
> `vitals.onnx`. A integracao Rust desta fase cobre o `occupancy.onnx`;
> presenca e vitais sao extensoes posteriores (o servidor ja tem heuristica e
> FFT proprias para esses casos). A **fonte canonica do contrato** e o
> `occupancy_metadata.json` gerado pela run — sempre confira o metadata da run
> aprovada em vez de confiar nos shapes deste documento.

## Gate de Entrada

Antes de iniciar qualquer alteracao em backend, frontend, MQTT ou Home Assistant, confirmar:

- `occupancy.onnx` foi exportado pelo pipeline `ml/` no Thunder Compute (treino completo, nao fast-dev).
- `occupancy_metadata.json` existe e define input `[1, 1, 64, 50]`, `max_count` e classes.
- `benchmark.json` / `metrics_summary.json` registram metricas de treino e validacao ONNXRuntime.
- O smoke test (`uv run pipeline.py --fast-dev` ou `main.py` opcao [1]) e o treino completo foram executados. O smoke local com dados reais passou em 2026-06-10; falta a execucao no Thunder.
- Nao houve coleta local de dados humanos.
- Os resultados foram anotados em `docs/Kanban.md`.

## Backend Rust

Adicionar inferencia ONNX somente depois do gate:

- Incluir `tract-onnx` como dependencia do `aethersense-server`.
- Criar um modulo de inferencia para carregar `models/occupancy.onnx`.
- Ler `models/occupancy_metadata.json` para validar shape, classes e tamanho do heatmap.
- Manter fallback heuristico quando o modelo nao existir, falhar ou receber historico insuficiente.
- Evitar segurar o `RwLock` global durante inferencia: coletar snapshot dos historicos, soltar lock, executar modelo e aplicar resultado em seguida.

Contrato esperado do modelo (com `max_count = 5`, o default atual — a ultima
classe significa "5 ou mais"; validar contra o `occupancy_metadata.json` da run):

```text
input:  csi          [1, 1, 64, 50]
output: count_logits [1, 6]          # classes 0, 1, 2, 3, 4, 5+
output: heatmap      [1, 1, 20, 20]
```

## Feature Extraction e Baseline

Implementar preparo do input live:

- Agregar historicos de amplitude por receptor ativo.
- Reamostrar subportadoras para 64.
- Cortar ou preencher tempo para 50 amostras.
- Aplicar subtracao de baseline de sala vazia quando disponivel.
- Normalizar com a mesma politica registrada no metadata do treino.

Extensao futura de baseline:

- Salvar media por subportadora.
- Salvar desvio padrao por subportadora.
- Preservar compatibilidade com configs antigas que so possuem media/variancia globais.

## Estado, API e WebSocket

Adicionar um campo `occupancy` em `DetectionState` depois da validacao:

```text
count: 0..5
probabilities: [p0, p1, p2, p3, p4, p5plus]
confidence
method
persons[]
heatmap
heatmap_points
```

Endpoints futuros:

- `GET /api/ml/status`
- `POST /api/ml/model`

O WebSocket deve continuar enviando `detection` como hoje, apenas com o novo subcampo `occupancy`.

## Mapeamento do Heatmap

Converter celulas do heatmap para coordenadas:

- Se houver `Floorplan` cadastrado, mapear a grade `20x20` para `bounds` da planta.
- Se nao houver planta, usar uma grade local padrao de `10m x 10m` ao redor dos receptores ativos.
- Extrair picos do heatmap como pessoas, com limiar minimo de confianca e supressao local para evitar duplicatas.

## Frontend

Depois do backend publicar `detection.occupancy`:

- Atualizar tipos TypeScript para `OccupancyEstimate` e `PersonEstimate`.
- Header deve priorizar contagem ML: `LIVRE`, `1 PESSOA`, `2 PESSOAS`, …, `5+ PESSOAS`.
- `AetherMap` deve renderizar marcadores por pessoa e overlay `leaflet.heat`.
- `SystemPipeline` deve exibir barras compactas para probabilidades `0 / 1 / 2 / 3 / 4 / 5+`.
- Preservar marcador unico atual como fallback quando `occupancy.persons` estiver vazio.

## MQTT e Home Assistant

Adicionar discovery apenas quando o contrato online estiver estavel:

- `sensor.aethersense_occupancy_count`
- `sensor.aethersense_occupancy_confidence`
- `sensor.aethersense_occupancy_method`
- slots fixos para `person_1..person_5` com lat/lng/confidence

Evitar publicar entidades dinamicas ilimitadas; Home Assistant funciona melhor com conjunto estavel de sensores.

## Testes Exigidos

Backend:

- Modelo ausente ativa fallback.
- Modelo invalido nao derruba o servidor.
- Feature extractor gera `[1, 1, 64, 50]`.
- Baseline antigo desserializa.
- Baseline novo salva stats por subportadora.
- Heatmap com picos conhecidos gera pessoas esperadas.
- API `/api/ml/status` reflete estado real.

Frontend:

- Build TypeScript passa.
- Mock WebSocket com `0`, `1`, `2`, `3`, `4` e `5+` pessoas renderiza corretamente.
- Heatmap sem floorplan usa grade padrao.
- Heatmap com floorplan respeita bounds.

Integracao:

- Copiar ONNX validado para `aethersense-server/models/`.
- Rodar `cargo test`.
- Rodar `npm run build`.
- Confirmar WebSocket com `detection.occupancy`.

## Observacao de Escopo

Esta fase futura nao autoriza coleta humana local. A unica adaptacao local permitida continua sendo calibracao de sala vazia.
