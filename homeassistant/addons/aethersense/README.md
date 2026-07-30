# AetherSense Home Assistant Add-on

Este diretório contém o manifesto do add-on do AetherSense para Home Assistant/Supervisor.

O add-on usa a mesma imagem do backend Rust e, quando está rodando dentro do Supervisor, lê as opções de `/data/options.json` e pega automaticamente as credenciais do serviço MQTT do Home Assistant.

Principais pontos:

- API HTTP em `8000/tcp`
- Ingestão UDP CSI em `5005/udp`
- MQTT discovery para `binary_sensor` e `sensor`, incluindo sensores individuais de localização
- Relés provisionados aparecem como `switch` não otimista; códigos IR salvos aparecem como `button`
- Comandos e ACKs trafegam com QoS 1 por tópicos próprios de cada ESP32
- Dependência do serviço `mqtt`
- Localização georreferenciada exposta pelo backend com `lat`, `lng`, `floor`, `accuracy_m`, `accuracy_dm`, `confidence`, `nodes_used`, `method`, `experimental` e `resolution_m`

Os testes automatizados cobrem o contrato do MQTT Discovery, incluindo zonas,
relés e botões IR. Resultados de bancada, portas seriais, endereços MAC e
coordenadas de instalações particulares não fazem parte da distribuição
pública.

Se você publicar a imagem no seu registry, esse manifesto pode ser usado como add-on privado ou em um repositório de add-ons do Home Assistant.

Preencha `api_token` nas opções do add-on e envie `Authorization: Bearer <token>` em toda mutação REST. Uploads retomáveis do microSD são a exceção: usam a credencial individual do dispositivo cadastrada no onboarding, e não esse token administrativo.

A senha do broker configurada na opção protegida `mqtt_password` é exportada ao processo como `AETHERSENSE_MQTT_PASSWORD`; o servidor não a copia para `aethersense_config.json`. Senhas enviadas diretamente ao endpoint MQTT são voláteis e precisam ser reenviadas após reinício.
