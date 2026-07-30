# Firmware de acessórios: TUI, IR, relés e microSD

Este documento descreve o contrato operacional da primeira versão de
acessórios do AetherSense. A configuração normal do usuário é feita por
`uv run build.py`; arquivos e variáveis permanecem apenas como interfaces de
automação e compatibilidade.

## Fluxo pela TUI

O fluxo normal abre na página **Início**:

1. Conecte a placa pelo cabo USB.
2. Se houver uma placa, pressione **Configurar e testar automaticamente**; se houver mais de
   uma, escolha apenas o nome amigável desejado.
3. A TUI ignora Bluetooth, reutiliza o perfil associado, identifica o chip/eFuse
   no primeiro uso e encadeia somente as etapas necessárias de build, flash,
   NVS e validação física, sem pedir comandos, hashes, target ou porta COM. O
   monitor só abre depois que a validação liberar a serial.

O perfil salvo preserva integralmente a fiação de IR, relés e microSD. Em placa
nova, periféricos externos ficam desativados porque USB não informa em qual GPIO
um fio foi conectado. A placa pareada `Meu espc5` é uma exceção conhecida e
imutável: RX GPIO8, TX GPIO9 e RGB integrado GPIO27 são recuperados
automaticamente. Um perfil antigo incompleto é reparado conservadoramente:
o recurso sem ligação salva é desativado, nunca ligado em um pino por palpite.

**Opções avançadas** mantém o comissionamento completo:

- **Seções** abre um menu vertical de cards; a antiga faixa horizontal de abas
  foi removida para não cortar opções em terminais pequenos. **Início** volta ao
  modo automático e as setas percorrem as sete etapas em ordem.

1. Crie, importe, clone ou selecione um dispositivo C5/WROOM no catálogo.
2. Associe a porta detectada ao serial USB e ao MAC eFuse do ESP32.
3. Configure Wi-Fi e CSI/UDP. MQTT e URL do servidor são opcionais: só são
   necessários para controle remoto/dashboard, não para testar os acessórios
   conectados por USB. Segredos ficam no keyring e aparecem mascarados.
4. Habilite IR RX/TX, relés e/ou SDSPI. Todo GPIO é explícito; conflito,
   direção incompatível, flash, strapping ou recurso RMT/SPI indisponível
   bloqueia a aplicação.
5. A tela de revisão informa se a alteração requer build/flash ou apenas
   provisionamento NVS. A TUI executa a sequência necessária e compara os
   hashes desejado e reportado pelo dispositivo.
6. Nos cards de comissionamento, use **Aprender sinal** e **Repetir última
   captura** diretamente pela USB. Relé e cartão também preferem USB; se a
   placa não estiver conectada, a TUI oferece a integração remota configurada.

Depois de aplicar, os cards **Boot/USB**, **LED RGB**, **Receptor IR**,
**Transmissor IR**, **microSD**, **Rede** e **CSI** são validados na placa
conectada. ACK eletrônico e observação humana são evidências separadas: cores,
pulso amarelo/magenta e resposta do aparelho nunca recebem aprovação automática.
O resultado fica em `resultados/firmware-hardware/<sessão>/session.json` com
schema `aethersense.hardware_validation.v1`.

Perfis exportados usam o schema `aethersense.device_profile.v1`, não contêm
segredos e podem ser importados como cópia para outro dispositivo.

## LED RGB de estado no ESP32-C5

Ao selecionar a variante `esp32-c5-devkitc-1`, a TUI oferece o LED RGB
endereçável integrado da placa. O [guia oficial da Espressif](https://docs.espressif.com/projects/esp-dev-kits/en/latest/esp32c5/esp32-c5-devkitc-1/user_guide.html)
identifica esse LED no GPIO27; como esse GPIO também é de *strapping*, o planejador permite seu
uso somente pelo LED pertencente à própria placa e continua bloqueando-o para
IR, relés, SD e demais periféricos.

O mapa padrão, editável na TUI, é:

- inicialização: laranja;
- conectado: verde;
- desconectado: azul;
- erro: vermelho;
- sinal recebido pelo IR RX: amarelo;
- sinal transmitido pelo IR TX: magenta;
- provisionamento: ciano;
- gravação no cartão: violeta.

As indicações de IR são pulsos temporários e distintos: o LED volta ao estado
anterior depois do evento. A prioridade é erro, transmissão IR, recepção IR,
provisionamento/gravação e, por fim, o estado-base de conectividade. O driver
usa uma fila limitada e uma tarefa própria, portanto uma animação ou mudança de
cor nunca executa no callback de CSI, IR ou MQTT.

## Segurança elétrica e de inicialização

- GPIO opera em 3,3 V. O LED IR deve ser acionado por transistor/MOSFET com
  resistor dimensionado; não deve ser alimentado diretamente pelo GPIO.
- Cargas ligadas à rede elétrica exigem módulo de relé certificado, isolado e
  instalado em invólucro apropriado.
- Todo relé é configurado inicialmente no nível físico inativo. A restauração
  do último estado ocorre somente após dois segundos e apenas quando o slot
  NVS, o schema e o hash da configuração são válidos.
- A configuração padrão mantém todos os periféricos desabilitados.

## MQTT e REST

Cada ESP usa credencial própria e pode acessar somente seus tópicos:

```text
aethersense/devices/<device_id>/command
aethersense/devices/<device_id>/ack
aethersense/devices/<device_id>/event
aethersense/devices/<device_id>/state
aethersense/devices/<device_id>/capabilities
aethersense/devices/<device_id>/availability
```

Comandos são QoS 1, não retidos e levam `request_id`, `device_id`, `action`,
`params`, `ttl_ms` e identificação do boot. Repetir o mesmo `request_id` com o
mesmo conteúdo é idempotente; reutilizá-lo com conteúdo diferente é erro.

O servidor só cria/publica um comando operacional depois de observar o
dispositivo online e receber `capabilities` ou `state` com `boot_id` não vazio
na conexão MQTT atual. Uma desconexão ou `availability=offline` invalida o
`boot_id`; até a nova identificação, a REST responde `device_offline` ou
`device_boot_unknown` sem enfileirar nada. Envelopes com `boot_id=null` ficam
restritos a fixtures de compatibilidade do parser e não saem do publisher.

Ações suportadas:

- `relay.set`: `{ "relay_id": "...", "state": true }`;
- `relay.pulse`: `{ "relay_id": "...", "duration_ms": 1000 }`;
- `ir.learn.start`, `ir.learn.cancel` e `ir.send`;
- `storage.record.start`, `storage.record.stop` e `storage.sync`.

O servidor mantém as leituras REST públicas e exige o Bearer administrativo
`AETHERSENSE_API_TOKEN` em toda mutação. Os endpoints de upload abaixo são a
exceção: usam a credencial individual do dispositivo. Os principais endpoints
são:

```text
POST /api/accessories/onboard
POST /api/accessories/commands
GET  /api/accessories/commands/<request_id>
GET/POST/PUT/DELETE /api/ir/codes[/<id>]
POST /api/ir/learn
POST /api/ir/send
POST /api/storage/uploads
PUT/GET /api/storage/uploads/<upload_id>
POST /api/storage/uploads/<upload_id>/complete
```

O painel do dashboard mantém o Bearer apenas em memória. O campo global logo
abaixo do cabeçalho autentica todas as mutações da sessão; recarregar a página
o apaga. A TUI mantém o token administrativo no keyring e o fornece ao servidor
sem gravá-lo no perfil, exportação, URL ou log.

## IR bruto

A primeira versão guarda alternadamente marcas e espaços em microssegundos,
com carrier padrão de 38 kHz e duty cycle de 33%. Não há interpretação NEC,
RC5 ou de estados de ar-condicionado; por isso códigos aprendidos aparecem no
Home Assistant como `button`, não como `climate`.

O comissionamento local usa extensões do protocolo
`aethersense.provision.v1`: a TUI verifica o hash aplicado, arma o RMT RX,
recebe o código bruto e pode reproduzi-lo imediatamente pelo RMT TX. Esse fluxo
não requer broker, servidor, token ou onboarding. O cancelamento e o timeout
desarmam o receptor, e falhas de RMT encerram a tentativa uma única vez para
não bloquear o console serial. MQTT continua sendo o caminho para operação
remota depois que o dispositivo for instalado.

No C5, o worker de IR e o bridge USB iniciam antes da associação Wi-Fi. Assim,
aprender/reproduzir continua disponível com roteador lento ou offline; rede e
MQTT não são pré-requisitos para o controle local.

## microSD e privacidade

O cartão é FAT32/SDSPI e não é formatado automaticamente. Cada sessão usa:

```text
/aethersense/sessions/<session-id>/session.json
/aethersense/sessions/<session-id>/events-*.jsonl
/aethersense/sessions/<session-id>/csi-*.csv
```

O manifesto `aethersense.firmware_sd_session.v1` registra dispositivo,
horários, finalidade, arquivos e SHA-256. A sincronização envia blocos de até
1 MiB, pode retomar pelo offset e só permite remover a origem depois da
confirmação integral. O destino canônico é
`resultados/firmware-sd/<session-id>`.

A tarefa `aether-sd` usa explicitamente prioridade FreeRTOS 2 (baixa), fila
limitada e lotes pequenos. Montagem FAT, rotação, escrita e upload nunca são
executados no callback CSI nem na tarefa de rede.

Cada bloco usa exatamente este contrato HTTP (nomes de cabeçalho não diferenciam
maiúsculas/minúsculas; o valor hexadecimal deve ter 64 caracteres):

```text
PUT /api/storage/uploads/<upload-id>
Authorization: Bearer <credencial-individual-do-dispositivo>
Content-Type: application/octet-stream
X-Upload-Offset: <offset-decimal-do-primeiro-byte>
X-Chunk-SHA256: <sha256-lowercase-dos-bytes-exatos-do-body>
```

O servidor não grava nem avança o offset quando `X-Chunk-SHA256` estiver
ausente/malformado (`400`) ou divergir do corpo (`422`,
`chunk_integrity_failed`). O SHA-256 do arquivo completo informado no início
continua sendo verificado na conclusão.

Eventos podem ser gravados continuamente. CSI bruto local exige início
explícito e finalidade `bench` ou `empty_room`; não deve ser coletado com
presença ou locomoção humana.

## Desenvolvimento e validação

- Firmware: sempre executar build/flash por `uv run build.py`.
- Python/TUI: `uv run pytest` dentro de `firmware-rust`.
- Backend: `cargo test` dentro de `aethersense-server`.
- Frontend: `npm run build` e `npm run lint` dentro de `frontend`.
- Evidências novas: `resultados/<categoria>/<sessão>/session.json`.
- Placa física pela CLI: `uv run build.py validate-hardware --device-profile <UUID>`.
