# Automação por Zonas + Home Assistant — Arquitetura Online

> **Última atualização**: 2026-07-15 (sessões auditáveis e reconfiguração segura)
> **Status**: canônico — implementação pronta; implantação e comissionamento
> físico da zona do box ainda precisam ser concluídos no ambiente local.
> Complementa `ml-pipeline-status.md` (treino/modelos) com a camada de
> decisão e automação ao vivo.

## Visão geral da cadeia ao vivo

```
ESP32-C5 (quarto)  ──┐ broadcast UDP :5005 (firmware AETHERSENSE_UDP_HOST=255.255.255.255)
ESP32-WROOM (banheiro)┘
        │
        ▼  Windows: Docker Desktop NÃO entrega broadcast ao container
relay nativo (Tarefa Agendada "AetherSense UDP Relay" → scripts/udp-relay.ps1)
        │  unicast 127.0.0.1:5006 → container :5005
        ▼  [Linux/HA: docker-compose.linux.yml usa network_mode: host — SEM relay]
aethersense-backend (Docker)
        │  loop de detecção 2 Hz:
        │   • heurísticas (presença, movimento, localização fusionada, FFT vitais)
        │   • ML ONNX via tract (occupancy CNN + vitals LSTM) — lado a lado
        │   • zonas por receptor (máquina de estados ADAPTATIVA — ver abaixo)
        │   • zona dedicada do box (WROOM positivo − C5 referência negativa)
        ▼
MQTT (mosquitto do compose) → Home Assistant (~/homeassistant, compose separado)
        │  discovery: ~25 entidades aethersense_* (object_id determinístico)
        ▼
Automações (automations.yaml, TODAS editáveis pela UI do HA)
```

## Zonas por receptor (o coração da automação)

**1 receptor = 1 cômodo.** O score de cada zona é a perturbação do canal em
relação ao baseline de sala vazia daquele receptor, calculado sobre janela
curta (40 amostras ≈ 2–4 s — `detection/location.rs::estimate_active_zone`).

A decisão binária `occupied` é tomada NO SERVIDOR
(`detection/zones.rs::ZoneTracker`) para o HA reagir sem espera:

| Regra | Mecanismo | Tempo típico |
|---|---|---|
| ENTRAR | score > limiar_entrada em ≥2 dos últimos 3 ticks | ~1,5 s |
| SEGURAR | perturbação OU respiração crível (FFT conf ≥ 0,3 **E** ML count ≥ 1) | — |
| SAIR | **mediana** dos últimos 20 ticks < limiar_saída | ~10–30 s |

**Limiares ADAPTATIVOS por zona** (lição do dia: limiares fixos calibrados no
quarto falham no banheiro): cada zona aprende seu piso de ruído (percentil 20
dos últimos ~2 min) e deriva `entrada = 3×piso`, `saída = 1,8×piso` (com
clamps e histerese garantida). Os limiares aprendidos são publicados em
`ZoneScore.on_threshold/off_threshold` (visíveis nos atributos do sensor
"Active Zone" no HA) para auditoria.

**Por que mediana na saída**: medição real mostrou 30% dos ticks vazios acima
do limiar antigo — a regra "todos os 20 ticks quietos" tinha ~0,1% de chance
por janela (a zona nunca saía). Mediana é robusta a picos.

**Por que a retenção por respiração é restritiva**: o VitalsNet é regressão
em faixa fisiológica — SEMPRE emite valor, não é evidência de presença. A FFT
espúria tem confiança ~0,2. Sem os critérios (conf ≥ 0,3 + ML concordando), a
zona ficava presa em "ocupada" para sempre (bug observado e corrigido).

### Zona dedicada do box do chuveiro

A válvula **não** usa mais diretamente a zona genérica do WROOM. O backend
mantém um `ShowerBoxTracker` independente: o WROOM, reposicionado próximo ao
box, é o sinal positivo; o C5 do quarto é a referência negativa. A entrada só
é evidência do box quando o score normalizado do WROOM, calculado em uma janela
curta de 0,5 s, supera seu limiar, a dominância
`score_wroom - score_c5` também supera o limiar correspondente **e há movimento
instantâneo corroborando a entrada**. Assim, uma perturbação forte no quarto ou
um offset estático do WROOM vazio não abre a válvula.

Antes de decidir presença, o tracker exige que os dois receptores estejam
prontos continuamente por 5 s. Para cada receptor, isso significa:

- baseline de sala vazia válido, com pelo menos 100 amostras; WROOM e C5 devem
  trazer o mesmo `calibration_session_id`, criado por um único `/api/clear`;
- última amostra CSI recebida há no máximo 1,5 s;
- taxa recente de chegada de CSI de pelo menos 4 Hz, calculada sobre os
  timestamps dos últimos 5 s (o campo PHY `rate` do pacote não é usado).

Somente `stream.source=udp_live` alimenta o tracker. Replay CSV e estado ocioso
resetam imediatamente a zona para `vacant`, com `ready=false`, e nunca podem
acionar a válvula real.

Enquanto qualquer requisito falhar, `ready=false` e uma nova entrada não pode
ser confirmada. A disponibilidade MQTT representa o servidor; a saúde de cada
receptor permanece nos atributos. Isso permite que uma perda breve de CSI
mantenha o estado `on` durante o hold de segurança, em vez de ocultá-lo como
`unavailable`. Com as entradas prontas, a máquina de estados é
`vacant → candidate → occupied → holding → vacant`:

| Fase | Regra |
|---|---|
| `vacant` | válvula sem autorização; apenas amostras claramente vazias atualizam os limiares adaptativos |
| `candidate` | votação temporal de WROOM forte, dominante sobre o C5 e movimento instantâneo; exige pelo menos 2/3, no mínimo quatro decisões e nenhum intervalo maior que 1 s na janela configurada (2,5 s no perfil equilibrado) |
| `occupied` | presença confirmada; publica `ON` para a automação |
| `holding` | evidência enfraqueceu ou uma entrada ficou indisponível, mas mantém `ON` pelo hold configurado (mínimo 60 s) desde a última evidência válida |
| retorno a `vacant` | hold completo sem WROOM forte + movimento instantâneo nem zona WROOM genericamente corroborada; publica um único `OFF` |

Os limiares do box aprendem somente com observações classificadas como
ambiente vazio; qualquer presença/corroboração impede aprendizado. Evidência
forte do tracker dedicado (`WROOM >= on` e dominância `>= on`) nunca basta
sozinha: movimento instantâneo é obrigatório para abrir. A zona genérica pode
apenas sustentar imobilidade depois de uma entrada legítima e ainda exige WROOM
acima do limiar `off`; respiração/ocupação ML não comandam esse intertravamento.

O comportamento é persistente e editável sem recompilar em
`GET/POST /api/shower-box/settings`. A aba comum do dashboard oferece perfis
Conforto, Equilibrado e Evitar disparos, além de sensibilidade, confirmação,
tolerância de entrada, reconfirmação, retenção e modos de comando.

Salvar esses ajustes não reinicia uma sessão já `occupied`/`holding`: o tracker
preserva a identidade da sessão, a última evidência e o relógio de retenção.
Somente uma tentativa ainda em `candidate` é descartada, para que votos obtidos
com limiares antigos não sejam reaproveitados depois da mudança.

Para tornar os testes atribuíveis sem gravar CSI ou trajetórias, o estado mantém
somente auditoria agregada e efêmera da última tentativa e da última sessão:
sequência, resultado, pico de confirmação, horários de início/fim e motivo do
encerramento. O dashboard traduz esses campos nos cartões **Última tentativa** e
**Sessão ativa/Última sessão**; os dados não são gravados na configuração nem em
`resultados/`.

No Windows, o relay UDP reserva `:5005` exclusivamente para o broadcast dos
ESP32, encaminha a produção para o backend em `:5006` e duplica um tap de teste
em `:5007`. A suíte usa replay por padrão e só abre o tap quando
`AETHERSENSE_REQUIRE_LIVE_CAPTURE=true` ou
`AETHERSENSE_TRY_LIVE_CAPTURE=true`; capturas ao vivo permanecem em memória, a
menos que a persistência seja solicitada explicitamente. Assim, testes não
competem com a automação nem criam CSVs locais silenciosamente.

O relay trabalha com prioridade `Normal`, reserva `:5005` com exclusividade e
publica somente contadores agregados em
`%LOCALAPPDATA%\AetherSense\udp-relay-health.json` (schema
`aethersense.udp_relay_health.v2`). Esse arquivo não contém payload CSI.

## Entidades no Home Assistant (discovery MQTT)

Principais para automação:

| Entidade | Fonte | Uso |
|---|---|---|
| `binary_sensor.aethersense_zona_esp32_c5` | ZoneTracker (quarto) | lâmpada |
| `binary_sensor.aethersense_zona_esp32_wroom` | ZoneTracker genérico (banheiro) | compatibilidade e diagnóstico; **não** controla a válvula |
| `binary_sensor.aethersense_zona_box` | ShowerBoxTracker (WROOM − C5) | válvula do chuveiro |
| `input_boolean.aethersense_chuveiro_sessao_automatica` | latch local do Home Assistant | autoriza fechar somente a sessão que a automação abriu |
| `binary_sensor.aethersense_occupancy` | ML-first (count>0; heurística só sem modelo) | geral |
| `sensor.aethersense_ml_person_count` | ML ONNX | contagem 0..5+ |
| `sensor.aethersense_active_zone` (+atributos `zones[]` com scores/limiares) | servidor | depuração/automação avançada |
| `sensor.aethersense_ml_breathing_rate` / `ml_heart_rate` | VitalsNet (availability-gated) | vitais ML |

Sensores ML numéricos usam *availability template* (ficam `unavailable` em
vez de mandar string para sensor numérico — exigência do HA).
`binary_sensor.aethersense_zona_box` usa a disponibilidade do servidor e
publica `ready` separadamente. Seus atributos expõem fase, scores, dominância,
limiares, taxas CSI, baselines, disponibilidade dos dois receptores, progresso
de entrada, segundos restantes de retenção, perfil, modo de comando, fontes de
evidência, motivo da última transição e os resumos agregados da tentativa/sessão
mais recentes (`attempt_*`, `session_*` e `last_session_*`).

## Automações (em `~/homeassistant/config/automations.yaml`)

v8 — gatilhos sobre decisões já estabilizadas no servidor e propriedade de
sessão no Home Assistant:

1. Lâmpada liga: zona quarto → `on` (sem `for:`)
2. Lâmpada desliga: zona quarto → `off`
3. Válvula liga: transição nova `off → on` da `zona_box`, pronta e confirmada,
   somente nos modos `automatic` ou `assisted`. Um `on` retido vindo de
   `unknown`/`unavailable` após reinício não rearma a válvula. Somente
   `automatic` liga o latch de propriedade somente depois da Tuya confirmar
   `on`; `assisted` já nasce manual
4. Válvula desliga: `zona_box` completa a retenção e permanece `off` por mais
   2 s, somente em `automatic` **e** com o latch provando que a automação abriu
5. Fail-safe: no boot do Home Assistant a válvula é fechada; indisponibilidade
   por 60 s só fecha uma sessão automática com latch ativo
6. Reconciliação (a cada 5 min): atua somente na lâmpada. Snapshot nunca liga ou
   desliga a válvula, evitando reabertura após boot/desligamento manual
7. Watchdog de fechamento (a cada 1 min): apenas repete fechamento perdido de
   sessão com latch automático; uma ligação manual jamais satisfaz a condição
8. Desligar a tomada ou mudar para `assisted/manual` limpa o latch e transfere
   definitivamente a sessão à pessoa; voltar a `automatic` não toma posse dela
9. Descanso noturno: presença + movimento `static` por 10 min entre 21h–9h →
   apaga a lâmpada (a zona CONTINUA ocupada — respiração segura; não religa)

Todas têm `id:` → totalmente editáveis pelo editor visual do HA
(Configurações → Automações; aba Rastreios para depurar disparos).

## Deploy (sem interação do usuário)

- **Windows (este PC)**: `docker compose up -d --build` no repo + Tarefa
  Agendada do relay (instalada uma vez por `scripts/install-relay-autostart.ps1`;
  PowerShell puro, logon, oculta, reinicia em falha). MQTT no compose
  (porta 1883); HA conecta via `host.docker.internal` (config entry injetada
  no `.storage` — backup `core.config_entries.backup-aethersense`).
- **Linux/Home Assistant**: `docker compose -f docker-compose.yml -f
  docker-compose.linux.yml up -d` → `network_mode: host`, broadcast direto,
  SEM relay. (Add-on HA: `host_network: true`.)

## Hardware — estado e lições

| Placa | Posição | IP (DHCP) | Firmware |
|---|---|---|---|
| ESP32-C5 | quarto, carregador de parede | era 192.168.18.74 | broadcast (2026-06-12) |
| ESP32-WROOM | banheiro, carregador | 192.168.18.23 | broadcast (2026-06-12) |

- **Broadcast no firmware** (`AETHERSENSE_UDP_HOST=255.255.255.255` no flash)
  = imune a mudança de IP do PC. Nunca mais flashar com IP fixo.
- **C5 no USB do PC = frágil**: abrir o COM (DTR ativo) derruba em modo
  DOWNLOAD do bootloader ("waiting for download"); cura = pulso de reset só
  no EN (BOOT solto) ou replug. Alimentar por carregador elimina o risco.
- **Comissionamento do WROOM junto ao box**: fixe a placa fora da área molhada,
  próxima à lateral/entrada do box, com o caminho rádio atravessando quem fica
  sob o chuveiro. Não altere posição, cabo nem orientação após calibrar.
- **Reposicionou o WROOM → descarte os dois baselines e medições antes de
  calibrar**: com banheiro, box e quarto vazios, execute primeiro
  `DELETE /api/location/baseline` e depois `POST /api/clear`. Aguarde **ambos**
  os receptores acumularem pelo menos 100 novas amostras ao vivo, recentes e a
  ≥4 Hz; somente então execute `POST /api/location/baseline`. A API rejeita
  replay, histórico anterior ao clear, receptor sem frescor/taxa e uma segunda
  calibração do mesmo receptor sem novo clear. Confirme na resposta
  `complete_for_shower_box=true`: sucesso parcial não libera o box. Baselines
  antigos, sem o ID comum dessa nova coleta, continuam válidos para as funções
  legadas, mas são rejeitados pelo `ShowerBoxTracker`.
- **Taxa CSI é refém do tráfego Wi-Fi** (CSI é passivo): 2 nós se estimulam
  mutuamente (~20 pps, teto do pacing 10 Hz/nó). Nó único em rede ociosa
  oscila 5–8 Hz → `tools/dev_traffic_gen.py` (mirar NOS ESPs — tráfego do PC
  em 5 GHz é invisível para eles em 2,4 GHz).

## Pendências conhecidas (2026-07-15)

1. **Validação funcional supervisionada pendente**: executar os 10 ciclos
   agregados descritos em `resultados/validacao-zona-box/README.md`; não criar
   `session.json` antes de concluir os dez ciclos.
2. **Perda física M7 acima da meta acadêmica**: em 2026-07-15 o WROOM mediu
   `5,73%`, `1,97%` e `2,63%` em três janelas de 300 pacotes (limite `1%`),
   enquanto ambos os receptores permaneceram perto de 10 Hz e acima do gate
   operacional de 4 Hz. A votação de entrada tolera drops, mas a causa de RF
   ainda deve ser investigada separadamente.
3. **Comissionamento final**: manter/fixar o
   WROOM, calibrar o par na mesma sessão vazia e executar os 10 ciclos agregados
   descritos em `resultados/validacao-zona-box/README.md`.
4. Não reduzir limiares para compensar geometria inadequada. Se os 10 ciclos
   falharem com o hardware fixo, a próxima solução é um terceiro receptor.

## Depuração rápida

```powershell
# Estado completo (zonas com scores, limiares e occupied):
Invoke-RestMethod http://127.0.0.1:8000/api/detection | ConvertTo-Json -Depth 10

# Comparação ML × heurística:
curl http://127.0.0.1:8000/api/ml/status

# O que o HA recebe:
docker exec aethersense-mqtt mosquitto_sub -t aethersense/detection/state -C 1

# Relay vivo?
Get-ScheduledTask -TaskName "AetherSense UDP Relay"

# C5 preso em download? (boot log)
powershell -File %TEMP%\c5_reset_run.ps1   # pulso de reset modo run
```
