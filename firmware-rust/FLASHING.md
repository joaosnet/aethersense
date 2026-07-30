# Gravando o firmware AetherSense no seu ESP32

Guia passo a passo para **qualquer pessoa gravar o firmware no próprio ESP32** e
usar o AetherSense, sem as instabilidades antigas (o dispositivo agora reconecta
sozinho ao Wi-Fi e se reinicia se travar — ver [Resiliência](#6-resiliência-reconexão-automática-e-auto-reboot)).

Há dois caminhos:

- **A — Binário pronto (sem toolchain):** baixe o `.bin` do Release e grave. Mais
  rápido se você não quer instalar Rust/ESP-IDF.
- **B — Build local (do código):** compile você mesmo. Necessário para alterar o
  firmware ou se não houver Release para a sua placa.

Placas suportadas (mesmo firmware, perfil diferente):

| Perfil | Placa | Chip | Target Rust | ESP-IDF |
| --- | --- | --- | --- | --- |
| `classic` | ESP32-WROOM (Wi-Fi 4, 2.4 GHz) | `esp32` | `xtensa-esp32-espidf` | v5.4.2 |
| `esp32c5` | ESP32-C5 (Wi-Fi 6, dual-band) | `esp32c5` | `riscv32imac-esp-espidf` | v5.5.4 |
| `esp32c6` | ESP32-C6/C61 (Wi-Fi 6, Thread/Matter) | `esp32c6` | `riscv32imac-esp-espidf` | v5.4.2 |

> Na TUI, escolha a placa no perfil do dispositivo. Na CLI, passe
> `--device-profile <UUID>`; o alvo canônico é lido desse perfil, sem edição de
> variável de ambiente.

---

## Caminho A — Gravar o binário pronto (sem toolchain)

Os binários são publicados automaticamente pelo CI a cada release (workflow
[`.github/workflows/firmware.yml`](../.github/workflows/firmware.yml)). Cada `.bin`
é uma **imagem única gravável a partir de `0x0`** (bootloader + tabela de partições
+ aplicação), então não precisa juntar arquivos.

1. Baixe `aethersense-<perfil>.bin` da página de **Releases** do projeto (ex.:
   `aethersense-classic.bin` para o WROOM, `aethersense-esp32c5.bin` para o C5).
2. Instale só o gravador (não precisa do Rust):
   ```bash
   cargo install espflash      # ou: pip install esptool
   ```
3. Grave a partir de `0x0`:
   ```bash
   # com espflash
   espflash write-bin 0x0 aethersense-classic.bin --port <PORTA>

   # ou com esptool
   esptool.py --chip esp32 --port <PORTA> write_flash 0x0 aethersense-classic.bin
   ```
   - `<PORTA>` é a porta serial: `COMx` no Windows, `/dev/ttyUSBx` ou
     `/dev/ttyACMx` no Linux, `/dev/cu.usbserial-*` no macOS.
   - Para o C5/C6, troque `--chip esp32` por `--chip esp32c5` / `--chip esp32c6`.

Pule para **[4. Configurar o Wi-Fi](#4-configurar-o-wi-fi-sem-recompilar-captive-portal)**.

---

## Caminho B — Build local (do código)

### 1. Toolchain (uma vez por máquina)

O `build.py` cuida do Python (via `uv`) e baixa o ESP-IDF automaticamente. Você só
precisa instalar:

1. **Rust** — https://rustup.rs
2. **Toolchain `esp`** (compilador para Xtensa e RISC-V dos ESP32):
   ```bash
   cargo install espup ldproxy
   espup install
   ```
   - **Linux/macOS:** depois rode `. $HOME/export-esp.sh` no shell antes de
     compilar (ou adicione ao seu `.bashrc`/`.zshrc`).
   - **Windows:** o `espup` já configura o ambiente; reabra o terminal.
   - O canal `esp` é fixado por [`rust-toolchain.toml`](rust-toolchain.toml); o
     `build.py` invoca `cargo +esp` automaticamente.
3. **uv** — https://docs.astral.sh/uv/getting-started/installation/
   - Linux/macOS: `curl -LsSf https://astral.sh/uv/install.sh | sh`
   - Windows: `irm https://astral.sh/uv/install.ps1 | iex`

> **Linux:** instale também as dependências de build do ESP-IDF
> (`git`, `cmake`, `ninja-build`, `python3-venv`, `libssl-dev`, `pkg-config`).
> Veja a lista oficial em https://docs.espressif.com/projects/esp-idf/en/stable/esp32/get-started/.

### 2. Configurar, compilar, gravar e provisionar pela TUI

Rode **a partir de `firmware-rust/`**:

```bash
uv run build.py
```

A tela inicial exige apenas conectar o cabo USB e escolher **Configurar e
executar**. A TUI ignora portas Bluetooth, encontra o perfil salvo mesmo quando
a porta `COM` mudou e calcula automaticamente se precisa parear, compilar,
gravar, provisionar ou apenas abrir o monitor. Com duas placas conectadas, pede
somente qual nome amigável deve ser usado.

Os detalhes ficam em **Opções avançadas**. Não há mais uma faixa horizontal de
abas: o card **Seções** abre uma lista vertical de cards para identidade, rede,
pinos/recursos, integrações, armazenamento, execução, diagnóstico e ajuda. O
rodapé compacto usa anterior/próxima e continua legível em terminal de 80
colunas. Ali também é possível parear manualmente; antes de ações de flash, a
identidade eFuse continua protegendo contra gravar outra placa.

Wi-Fi, MQTT e Bearer são digitados em campos protegidos e ficam no Credential
Manager/keyring. Eles não entram em argumento, variável do Cargo, `sdkconfig`,
perfil exportado ou log. Rede, pinos, cores do LED e políticas operacionais são
gravados atomicamente nos dois slots NVS pelo protocolo
`aethersense.provision.v1`; uma interrupção mantém a configuração anterior.

A TUI também permite IR bruto RX/TX, até oito relés, SDSPI, broker local/Home
Assistant/externo, onboarding do servidor, importação de SD e aplicação em lote.
O modo simples reutiliza os GPIOs já salvos e só aplica automaticamente uma
ligação externa quando existe um mapa imutável da placa pareada. Para `Meu
espc5`, usa IR RX GPIO8 e IR TX GPIO9. No perfil suportado do
ESP32-C5-DevKitC-1, o WS2812 integrado usa GPIO27; o mesmo GPIO permanece
proibido para outros periféricos.

Os cards de comissionamento físico usam primeiro a USB: **Aprender sinal** arma
o receptor e **Repetir última captura** testa o transmissor sem MQTT, servidor,
token ou onboarding. Integrações de rede permanecem opcionais para o controle
remoto cotidiano. Se o runtime ainda estiver iniciando, a TUI aguarda e repete
a consulta automaticamente.

No Windows, cada fonte é espelhada em
`C:\afw\<repo>\<perfil>\<processo>` e o cache Cargo reutilizável fica em um
caminho curto `C:\a\<hash>`. O ESP-IDF global nunca é alterado. `sdkconfig` e
`sdkconfig.defaults` são regenerados de fontes canônicas em todo build.

### CLI não interativa (CI/automação)

CLI e TUI usam o mesmo perfil. Prefira UUID do catálogo local; JSON/ZIP individual
também é aceito. Segredos são resolvidos por referências no keyring, nunca por
flags:

```bash
uv run build.py build --device-profile <UUID>
uv run build.py flash --device-profile <UUID>
uv run build.py provision --device-profile <UUID>
uv run build.py validate-hardware --device-profile <UUID>
uv run build.py test-integration --device-profile <UUID> --integration all
uv run build.py inspect-sd --sd-path <PONTO-DE-MONTAGEM>
uv run build.py import-sd --sd-path <PONTO-DE-MONTAGEM>
uv run build.py test
```

`validate-hardware` sempre usa a placa USB real e grava evidência em
`resultados/firmware-hardware/<sessão>/session.json`. Confirmações de LED,
controle remoto, aparelho e sala vazia continuam manuais; mocks nunca promovem
o perfil para **Sincronizado e testado**.

`build`, `flash`, `run`, `monitor`, `rescue`, `erase`, `clean`, `fullclean` e
`flashall` continuam disponíveis. Ações destrutivas exigem perfil pareado por
padrão. `--allow-unpaired-legacy` existe apenas para recuperação deliberada e
mostra um aviso porque não pode garantir a identidade eFuse.

### Aplicar em vários dispositivos

Use **Dispositivos → Planejar lote → Aplicar lote**. A TUI resolve a porta atual
por serial USB (ou VID/PID seguido obrigatoriamente da conferência eFuse), mostra
itens bloqueados, pede confirmação e executa onboarding, flash, provisionamento e
readback sequencialmente. O antigo `flashall` sem catálogo só permanece para
recuperação explícita: `uv run build.py flashall --allow-unpaired-legacy`.

---

## 3. Verificar (qualquer caminho)

Abra o monitor serial e confirme o boot e o streaming:

```bash
uv run build.py monitor --port <PORTA>
```

Você deve ver `AetherSense firmware boot: ...`, depois `UDP destination resolved
to ...` e `Transmitted N frames`. Com o backend rodando, o dispositivo aparece em
`GET /api/devices` e nos logs (`docker logs aethersense-backend`).

---

## 4. Configurar o Wi-Fi (sem recompilar) — captive portal

Não é preciso colocar SSID/senha no código. Ao ligar **sem credenciais salvas**, o
ESP sobe um ponto de acesso de configuração:

1. No celular/PC, conecte-se à rede Wi-Fi **`AetherSense-Config`** (aberta).
2. Abra o navegador em **http://192.168.71.1** (a maioria dos celulares abre a
   página de captive portal sozinha).
3. Informe o **SSID** e a **senha** da sua rede e salve.
4. O dispositivo grava as credenciais na NVS e **reinicia**, conectando à sua rede.

Para trocar de rede depois, reprovisione pela TUI. Em recuperação, apague a flash
(`uv run build.py erase --device-profile <UUID> --port <PORTA>`)
ou regrave — o portal volta a aparecer quando não há credenciais.

O captive portal é um fallback quando o keyring não está disponível ou o
dispositivo ainda não recebeu credenciais; o fluxo normal é provisionar pela TUI.

---

## 5. Descoberta do servidor (sem configurar IP)

O destino UDP padrão é **broadcast (`255.255.255.255:5005`)**: o firmware encontra
o servidor sozinho na LAN, sem você configurar IP. Basta o backend estar na **mesma
rede/sub-rede** que o ESP.

Dois detalhes de infraestrutura do servidor:

- **Linux:** rode o backend com rede de host
  (`docker compose -f docker-compose.linux.yml up`) para receber o broadcast.
- **Windows + Docker Desktop:** o Docker não entrega broadcast ao container. Rode o
  relay incluso, que reencaminha o broadcast `:5005` para o backend:
  ```powershell
  uv run tools/dev_udp_relay.py
  ```
  O relay também duplica os datagramas para o tap local `:5007`, usado apenas
  pelos testes de integração. Isso impede que um teste abra novamente `:5005`
  com `SO_REUSEADDR` e retire pacotes do backend. `cargo test` usa replay por
  padrão; para validar o hardware explicitamente, execute
  `$env:AETHERSENSE_REQUIRE_LIVE_CAPTURE='true'; cargo test` no servidor.
  Mesmo nesse modo o CSI fica somente em memória, salvo se
  `AETHERSENSE_SAVE_LIVE_CAPTURE=true` for definido deliberadamente em uma
  coleta permitida. (Detalhes no README da raiz.)

Se precisar fixar o destino (ex.: sub-redes diferentes), altere **UDP host** na
etapa Rede da TUI e aplique o perfil por provisionamento NVS; não é necessário
recompilar.

---

## 6. Resiliência (reconexão automática e auto-reboot)

O firmware monitora a própria saúde e se recupera sozinho de quedas de Wi-Fi —
você **não precisa mais reiniciar o ESP na mão**. Comportamento:

- **Reconexão automática (L2 + L3):** se a associação Wi-Fi cair, o firmware
  reassocia; se a associação está de pé mas o IP/lease DHCP se perdeu, ele força a
  renovação do DHCP. Ao voltar, o streaming UDP continua.
- **Auto-reboot de segurança:** se a conexão ficar insalubre por tempo demais, ou
  se parar de enviar UDP (tráfego/CSI travado), o ESP se reinicia para sair de
  qualquer estado preso.
- **Captive portal com timeout:** se houver credenciais salvas mas a rede estiver
  indisponível no boot, o portal de config reabre **com timeout** e o device
  re-tenta a rede salva sozinho — não fica preso no AP até o power-cycle. (No setup
  inicial, sem credenciais, o portal aguarda indefinidamente, como esperado.)

A imagem usa estes limites de segurança; configuração operacional do dispositivo
é persistida por provisionamento, sem variáveis manuais de build:

| Parâmetro | Padrão | Efeito |
| --- | --- | --- |
| Reconexão automática | ligada | Reconecta L2 e L3. |
| Verificação de saúde | `5 s` | Intervalo entre verificações. |
| Reinício por rede insalubre | `300 s` | Dá folga para reboot de roteador/DFS. |
| Reinício por envio parado | `180 s` | Recupera tráfego/CSI travado. |
| Timeout do captive portal | `180 s` | Re-tenta a rede salva quando já há credenciais. |

---

## 7. Notas específicas do ESP32-C5

O C5 é a placa mais nova e a mais sensível:

- Usa **ESP-IDF v5.5.4** (o `build.py` força isso para o perfil `esp32c5`).
- A entrada em **modo download por USB é frágil**: alimente a placa por um
  carregador/cabo firme e, se a gravação falhar, segure `BOOT`, toque `RESET` e
  solte `BOOT`.
- O `build.py` usa um **baud de flash mais conservador** automaticamente no C5.
- Em loop de crash, use `uv run build.py rescue --device-profile <UUID> --port <PORTA> --timeout 20` para
  recuperar a placa antes de regravar.
- Banda e preferência HT40 do C5 são escolhidas na etapa Rede da TUI e aplicadas
  pelo provisionamento NVS.

## 8. microSD e sincronização

O schema v1 usa FAT32/SDSPI e blocos retomáveis de exatamente **1 MiB
(1.048.576 bytes)**. A TUI exige GPIOs explícitos, URL `http(s)` e microSD
habilitado antes de ativar sincronização de rede. A importação física verifica
manifesto e SHA-256 e deduplica em `resultados/firmware-sd/`.
