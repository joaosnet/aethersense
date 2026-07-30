# AetherSense e Home Assistant

Este documento registra o estado da integracao com Home Assistant depois da entrega do backend Docker + MQTT + add-on.

## O que foi implementado

- O backend Rust agora pode publicar MQTT discovery e estado compacto para o Home Assistant.
- O backend também expõe a localização georreferenciada atual em `GET /api/detection`, incluindo `lat`, `lng`, `floor`, `accuracy_m`, `accuracy_dm`, `confidence`, `nodes_used`, `method`, `experimental` e `resolution_m`.
- O MQTT discovery publica sensores individuais para `latitude`, `longitude`, `accuracy`, `confidence`, `nodes_used` e `method`, além dos sensores já existentes de presença, movimento, RSSI e vitais.
- O discovery também publica cada relé como `switch` não otimista e cada código IR bruto salvo como `button`. Remoções geram tombstones MQTT retidos para não deixar entidades órfãs no Home Assistant.
- O backend assina `aethersense/devices/+/ack|event|state|capabilities|availability` e publica comandos não retidos em `aethersense/devices/{device_id}/command`, sempre com QoS 1.
- Comandos originados pelo Home Assistant entram em `aethersense/server/command`; o servidor acrescenta `request_id`, `boot_id`, `issued_at_ms` e TTL antes de encaminhá-los ao ESP32.
- Toda mutação REST exige `Authorization: Bearer <AETHERSENSE_API_TOKEN>`. A exceção são os uploads retomáveis do microSD, autenticados pela credencial individual do dispositivo. O add-on expõe o token administrativo pela opção `api_token` sem incluí-lo em snapshots ou logs.
- No dashboard, esse Bearer é informado no campo global da sessão e mantido somente em memória; ele é descartado ao recarregar a página.
- O entrypoint `run.sh` le `/data/options.json` quando roda como add-on.
- Quando `SUPERVISOR_TOKEN` existe, o entrypoint tenta descobrir automaticamente o servico MQTT do Home Assistant Supervisor.
- O manifesto do add-on ficou em `homeassistant/addons/aethersense/config.yaml`.
- O pacote REST de compatibilidade continua disponivel em `homeassistant/packages/aethersense.yaml`.

## Validacoes feitas

- `cargo check` do backend Rust passou sem erros.
- O script `run.sh` foi validado com `sh -n` dentro de um container Alpine.
- A publicação MQTT discovery foi validada por testes automatizados contra um broker MQTT mock local, incluindo sensores existentes, localização e entidades dinâmicas de relé/IR.
- Resultados de bancada e identificadores da instalação são mantidos fora da
  distribuição pública; os testes do repositório verificam os contratos de
  software sem depender desses dados.

## Resultado prático

A instalacao no Home Assistant pode seguir dois caminhos:

1. Add-on com MQTT discovery, usando a imagem do backend e o Supervisor para obter o broker MQTT.
2. Fallback via pacote REST, caso o usuario queira usar somente sensores e automacoes manuais.

## Recursos e endpoints

- `GET /api/accessories`: capabilities, disponibilidade e estado atual dos periféricos, com `accessory_version` também presente no WebSocket.
- `POST /api/accessories/onboard`: registra o dispositivo, seus recursos e opcionalmente uma credencial individual para upload.
- `POST /api/accessories/commands` e `GET /api/accessories/commands/{request_id}`: envio idempotente e acompanhamento de ACK, falha ou expiração.
- `GET|POST /api/ir/codes` e `GET|PUT|DELETE /api/ir/codes/{id}`: biblioteca persistente de sinais IR brutos.
- `POST /api/ir/learn` e `POST /api/ir/send`: aprendizado e reprodução pelo mesmo fluxo de comandos.
- `POST /api/storage/uploads`, `PUT /api/storage/uploads/{upload_id}` e `POST /api/storage/uploads/{upload_id}/complete`: upload retomável em blocos de 1 MiB para `resultados/firmware-sd/`. Cada `PUT` exige `X-Upload-Offset` e `X-Chunk-SHA256`; a conclusão também confere o SHA-256 integral.

As configurações não secretas do broker são persistidas, mas a senha MQTT nunca entra em `aethersense_config.json`. Ela deve vir de `AETHERSENSE_MQTT_PASSWORD` (a opção protegida `mqtt_password` do add-on exporta essa variável). Uma senha enviada pela API vale apenas para a execução atual; `GET /api/mqtt/settings` retorna somente `password_set` e `password_source`.

Migração: ao encontrar o campo legado `mqtt.password`, o servidor o remove por troca atômica preservando campos desconhecidos do JSON. Para evitar cortar uma conexão durante a atualização, o valor removido continua apenas na memória daquela execução; no próximo reinício é obrigatório configurar a variável de ambiente. Se o arquivo não puder ser saneado, o boot falha fechado em vez de continuar com o segredo no disco. Quando a senha vem do ambiente, o JSON pode guardar somente `password_secret_ref: "env:AETHERSENSE_MQTT_PASSWORD"`.

CSI bruto só é aceito quando o início do upload declara `capture_context` como `empty_room` ou `bench`; a API rejeita coleta humana local.

Para automações espaciais, use a localização como sinal probabilístico, não
como coordenada perfeita. Avalie a precisão no próprio ambiente e condicione
as automações aos campos de confiança e precisão publicados pelo servidor.

## Arquivos relevantes

- `aethersense-server/src/mqtt.rs`
- `aethersense-server/run.sh`
- `homeassistant/addons/aethersense/config.yaml`
- `homeassistant/packages/aethersense.yaml`
- `README.md`
- `frontend/src/components/HardwareTopology.tsx`
- `frontend/src/components/AetherMap.tsx`

## Observacao

A imagem referenciada no manifesto do add-on precisa ser publicada no registry informado antes do uso em um Home Assistant real.
