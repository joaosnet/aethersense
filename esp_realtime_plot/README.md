ESP Real-Time Plot

Dashboard em tempo real dos dados recebidos do ESP32 pela serial, com controles interativos, uma tabela `rich` no terminal com pacotes CSI em colunas e cinco visões analíticas do CSI.

## Como usar

1. Conecte o ESP32 na porta serial.
2. Ajuste a porta e o baud rate, se necessário.
3. Execute:

```bash
uv run main.py --port <SERIAL_PORT> --baud 115200 --window 4096 --history 20000
```

Use a mesma porta detectada durante o *flash* ou o monitoramento do firmware.
No Windows ela terá o formato `COMn`; no Linux, normalmente
`/dev/ttyUSBn`. O dashboard serial é uma ferramenta de depuração local.

Ao encerrar o programa, ele salva os arquivos finais em `exports/`:

- `*_raw.log` com as linhas recebidas pela serial.
- `*_packets.csv` com os campos estruturados do `CSI_DATA`.
- `*_samples.csv` com os valores numéricos do payload.
- `*_anim.gif` com a animação da curva, quando houver frames suficientes.
- `*_dashboard.png` com a captura do dashboard no encerramento.

No Linux/macOS, use algo como:

```bash
uv run main.py --port /dev/ttyUSB0 --baud 115200
```

## Como controlar

- Ajuste `Heatmap` para mudar quantos pacotes entram no mapa de calor.
- Ajuste `STFT` para definir a janela temporal usada no espectrograma.
- Ajuste `PCA` para mudar quantos pacotes entram na decomposição por componentes principais.
- Ajuste `Padding` para aumentar ou reduzir a margem vertical automática.
- Clique em `Pausar` ou pressione espaço para congelar o dashboard.
- Clique em `Limpar` ou pressione `c` para limpar os históricos.

## Gráficos

- Mapa de calor da amplitude das subportadoras, com RSSI sobreposto.
- Espectrograma STFT do sinal de movimento.
- Fase bruta comparada com a fase sanitizada.
- Curvas das componentes principais do CSI.
- Perfil de atraso de potência por IFFT.

## Formato esperado

O script tenta extrair valores numéricos das linhas brutas enviadas pelo ESP32.

Se o firmware estiver imprimindo `CSI_DATA,...,"[1,2,3,...]"`, o dashboard vai usar os valores do payload para alimentar as análises de amplitude, fase e PDP. A tabela do terminal mostra os pacotes CSI já separados em colunas.

## Dependências

- matplotlib
- numpy
- pyserial
- rich
