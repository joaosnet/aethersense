# Backend AetherSense

Servidor Rust responsável por receber quadros CSI via UDP, manter séries
temporais, executar processamento de sinais, carregar modelos ONNX e publicar
REST, WebSocket e MQTT.

## Executar

```bash
cp aethersense_config.example.json aethersense_config.json
cargo run
```

Valores padrão:

- API HTTP: `0.0.0.0:8000`;
- CSI: UDP `5005`;
- modelos: `models/`;
- configuração persistente: `aethersense_config.json`.

Use variáveis `AETHERSENSE_*` para alterar portas, diretórios e credenciais.
Senhas e tokens nunca devem ser escritos no JSON persistente.

## Testes

```bash
cargo test --locked --lib
```

Esse comando executa os 176 testes unitários e de contrato independentes de
hardware. Os ensaios `M1`–`M7` exigem receptores conectados ou capturas
fornecidas pelo próprio usuário. Capturas locais em `tests/captures/` são
ignoradas pelo Git.

## API

Depois de iniciar:

- `GET /api/health`;
- `GET /api/detection`;
- WebSocket em `/ws`.

A arquitetura completa está em
[`docs/arquitetura-firmware-servidor.md`](../docs/arquitetura-firmware-servidor.md).
